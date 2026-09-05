//! End-to-end test over a real iroh transport.
//!
//! The duplex-based tests in `network.rs` cover session logic; this covers the
//! parts they can't: ALPN negotiation, stream direction (`open_bi` on the guest
//! vs `accept_bi` on the host), and passcode validation across a real connection.
//!
//! Requires network access for the n0 relay, so it is `#[ignore]`d by default:
//!
//! ```bash
//! cargo test -p tain-core --test e2e_iroh -- --ignored --nocapture
//! ```

use anyhow::{Context, Result};
use tain_core::codec_h264::H264Decoder;
use tain_core::network::{read_stream_message, write_stream_message, SessionConfig};
use tain_core::{
    ApprovalDecision, CaptureConfig, ConnectionCode, ControlGrants, EncodeConfig, GuestClient,
    HostServer, Message, QualityScale, VideoCodec,
};
use std::sync::Arc;
use std::time::Duration;

/// Small frames keep the test fast and well inside the message cap.
fn test_config() -> SessionConfig {
    SessionConfig {
        capture: CaptureConfig {
            display: Default::default(),
            fps: 30,
            quality: QualityScale::Native,
            show_cursor: false,
            native_override: Some((64, 36)),
        },
        encode: EncodeConfig::default(),
        approval_timeout: Duration::from_secs(5),
        auto_regen_guest_passcode: true,
        exclude_audio_processes: Vec::new(),
        audio_bitrate_kbps: 160,
        host_name: Some("Test Host".into()),
        priority: Default::default(),
        // These stream a synthetic frame with no display behind it; a test
        // that resized the developer's desktop would be memorable.
        virtual_display_follows_guest: false,
    }
}

/// Next message from the host, or `None` on timeout / clean end of stream.
async fn next_message(recv: &mut iroh::endpoint::RecvStream) -> Option<Message> {
    let bytes = tokio::time::timeout(Duration::from_secs(30), read_stream_message(recv))
        .await
        .ok()?
        .ok()??;
    bincode::deserialize::<Message>(&bytes).ok()
}

fn decode_endpoint_id(hex_str: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_str).context("endpoint id is not valid hex")?;
    let arr: [u8; 32] = bytes.as_slice().try_into().context("endpoint id is not 32 bytes")?;
    Ok(arr)
}

/// Bring up a host, returning its handle, a master passcode, and its endpoint id.
async fn start_host() -> Result<(Arc<HostServer>, String, [u8; 32])> {
    let server = Arc::new(HostServer::bind().await?.with_config(test_config()));
    let passcode = server.generate_master_passcode().await?;
    let id = decode_endpoint_id(&server.endpoint_id())?;

    tokio::spawn({
        let server = Arc::clone(&server);
        async move {
            if let Err(e) = server.run().await {
                eprintln!("host server stopped: {e}");
            }
        }
    });

    Ok((server, passcode, id))
}

/// Where this host can actually be dialled from this machine.
///
/// `bound_sockets` reports what the endpoint bound, which is a wildcard —
/// `0.0.0.0:60617` and `[::]:60619`. Handing those to a guest as an address to
/// try is meaningless, and the first version of this test did exactly that: it
/// passed anyway, over the relay, while claiming to need no relay. It only
/// failed once the relay stopped answering, which is the condition it was
/// supposed to be testing under.
///
/// Substituting loopback for the unspecified address gives the guest something
/// it can really dial, and takes the relay out of the path for good.
fn loopback_addrs(server: &HostServer) -> Vec<std::net::SocketAddr> {
    server
        .local_addrs()
        .into_iter()
        .filter(|addr| addr.is_ipv4())
        .map(|mut addr| {
            if addr.ip().is_unspecified() {
                addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
            }
            addr
        })
        .collect()
}

/// Several guests arriving at once must all be answered.
///
/// # The failure this is for
///
/// The accept loop used to complete each QUIC handshake inline, so the host
/// could only shake hands with one guest at a time and everyone else waited
/// behind them — not for a moment, but until that handshake finished or timed
/// out. On a link where handshakes stall that is most of them, and from the
/// guest it looks like the host ignoring them at random. The host log said
/// "Failed to accept QUIC connection: timed out" and nothing about the attempts
/// that died queued behind it.
///
/// # Why this one needs no relay
///
/// It dials the host's own socket, which is the path a guest takes once it has
/// connected before. That makes it the only test in this file that runs on a
/// machine which cannot reach the n0 relay — which is the machine this software
/// is written for, and increasingly the one it is developed on.
#[tokio::test(flavor = "multi_thread")]
async fn several_guests_arriving_together_are_all_answered() -> Result<()> {
    let (server, passcode, host_id) = start_host().await?;
    let addrs = loopback_addrs(&server);
    assert!(!addrs.is_empty(), "the host bound no dialable socket");
    let key = iroh::PublicKey::from_bytes(&host_id).context("host id")?;

    // Enough to notice serialisation, few enough to stay quick.
    const GUESTS: usize = 4;
    let mut dials = Vec::new();
    for _ in 0..GUESTS {
        let (key, addrs, passcode) = (key, addrs.clone(), passcode.clone());
        dials.push(tokio::spawn(async move {
            let mut guest = GuestClient::connect_via(&key, None, &addrs).await?;
            let (_send, mut recv) = guest.authenticate(&passcode, Some("Crowd")).await?;
            // Reaching SessionStart is the proof: the handshake completed, the
            // stream opened and the host answered on it.
            let bytes = tokio::time::timeout(Duration::from_secs(20), read_stream_message(&mut recv))
                .await
                .context("timed out waiting for SessionStart")??
                .context("stream closed before SessionStart")?;
            anyhow::ensure!(
                matches!(bincode::deserialize::<Message>(&bytes)?, Message::SessionStart(_)),
                "first message was not SessionStart"
            );
            Ok::<_, anyhow::Error>(())
        }));
    }

    let mut answered = 0;
    let mut failures = Vec::new();
    for dial in dials {
        match dial.await.context("dial task panicked")? {
            Ok(()) => answered += 1,
            Err(e) => failures.push(format!("{e:#}")),
        }
    }

    assert_eq!(
        answered, GUESTS,
        "only {answered} of {GUESTS} guests were answered: {failures:?}"
    );
    Ok(())
}

/// A host nobody can place *yet* is not a host that is offline.
///
/// The host does not exist when the dial starts and appears ten seconds later.
/// Nothing about the link is slow; what is slow is the host becoming findable
/// at all, and the dial has to survive that and end up on a direct path.
///
/// # What this does and does not cover
///
/// It passes with or without the `NoAddress` retry in `connect_reporting`,
/// which is worth stating plainly because the first version of this comment
/// claimed otherwise. Instrumenting the loop showed the dial never failed at
/// all: `connect` waits on lookups still in flight, mDNS keeps its stream open,
/// and the host was picked up on the first attempt.
///
/// So what this pins down is that waiting — iroh's, and everything attached in
/// `discovery` — actually works end to end, which is worth a test in its own
/// right and would catch a regression that made a dial give up early.
///
/// The retry itself covers the narrower case where every lookup *completes*
/// with nothing, and the evidence for that one is
/// `an_endpoint_can_be_found_through_the_dht_alone`, which needed two asks.
/// Reproducing it here would take a lookup service that can be made to answer
/// on cue, and a fabricated one would only be testing the fake.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_appears_late_is_still_found() -> Result<()> {
    // A known identity, so the guest can dial a host that does not exist yet.
    let identity: [u8; 32] = rand::random();
    let key = iroh::SecretKey::from_bytes(&identity).public();

    let dialing = tokio::spawn(async move {
        // No relay url and no remembered address: every way of placing this
        // host has to come from discovery, and none of them can answer yet.
        GuestClient::connect_via(&key, None, &[]).await
    });

    tokio::time::sleep(Duration::from_secs(10)).await;
    let server = Arc::new(HostServer::bind_with_identity(Some(identity)).await?);
    tokio::spawn({
        let server = Arc::clone(&server);
        async move {
            if let Err(e) = server.run().await {
                eprintln!("host server stopped: {e}");
            }
        }
    });

    let guest = tokio::time::timeout(Duration::from_secs(45), dialing)
        .await
        .context("the dial never returned")?
        .context("dial task panicked")?
        .context("gave up on a host that became findable ten seconds in")?;

    let path = guest.connection_path().context("connected with no path")?;
    assert!(path.is_direct(), "found late, but ended up on {}", path.remote());

    // Deliberately not asserted: *which* lookup answered. Every service races,
    // and this test has a working relay and working DNS alongside multicast —
    // so mDNS winning is likely, not guaranteed, and under the load of the rest
    // of this file running beside it, DNS has been observed getting there
    // first. Attribution is pinned down in the test above instead, where
    // multicast is the only thing that can possibly answer.
    eprintln!("found late via {:?}", guest.found_via());
    Ok(())
}

/// Can this machine reach the mainline DHT at all?
///
/// # Why this exists separately
///
/// The round trip below can fail for two quite different reasons — the network
/// does not carry DHT traffic, or the record simply had not propagated yet —
/// and they want opposite responses. This asks the narrower question with
/// nothing else in the way: build a DHT node, and see whether it finds any
/// peers to talk to.
///
/// A `false` here means distributed discovery is not available on this network
/// and the round trip below cannot possibly pass. A `true` means it should.
///
/// ```bash
/// cargo test -p tain-core --test e2e_iroh reach_the_dht -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "talks to public DHT bootstrap nodes"]
async fn can_this_machine_reach_the_dht() -> Result<()> {
    let dht = n0_mainline::Dht::builder().build().context("could not build a DHT node")?;

    let started = std::time::Instant::now();
    let ok = tokio::time::timeout(Duration::from_secs(60), dht.bootstrapped())
        .await
        .context("bootstrapping did not finish within 60s")?
        .context("the DHT actor shut down while bootstrapping")?;

    eprintln!("DHT bootstrapped: {ok} (after {:?})", started.elapsed());
    if ok {
        let peers = dht.to_bootstrap().await.unwrap_or_default();
        eprintln!("routing table holds {} peers", peers.len());
    }
    assert!(
        ok,
        "this machine could not reach the mainline DHT — distributed discovery will not work \
         here, and the address lookup that depends on it is doing nothing"
    );
    Ok(())
}

/// The DHT must actually answer, not merely start.
///
/// # Why this is separate, and ignored
///
/// The mDNS test proves local discovery works and deliberately proves nothing
/// about the DHT — it returns in under a second, which is far too fast for a
/// DHT lookup, so whatever answered there, it was not this. And `attach` only
/// reports that the DHT *started*, which is a much weaker claim than that a
/// lookup through it comes back.
///
/// This closes that gap, and it needs the real public mainline DHT to do it:
/// bootstrapping against live nodes, publishing a signed record, and reading it
/// back from a second, independent DHT node. That is real traffic to real
/// third-party machines, which is why it does not run by default.
///
/// It is also the only way to answer the question that made the DHT worth
/// adding: whether this particular network lets that traffic through at all.
///
/// ```bash
/// cargo test -p tain-core --test e2e_iroh through_the_dht -- --ignored --nocapture
/// ```
///
/// # How it isolates the DHT
///
/// The publisher is an ordinary endpoint: it has a relay, and the DHT publishes
/// which relay that is (the crate's default filter is `relay_only`, so an
/// endpoint with no relay would publish nothing worth finding — which is why
/// this cannot use the bare `Minimal` preset the mDNS test uses).
///
/// The finder is where the isolation happens: every address lookup the N0
/// preset installed is cleared, and only the DHT is put back. It keeps a relay
/// transport and a DNS resolver, because it still has to *reach* whatever relay
/// it is told about — but it has no way left to learn *which* relay that is
/// except by asking the DHT. mDNS is not attached, so a local shortcut cannot
/// stand in for the answer either.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "publishes to and reads from the public mainline DHT"]
async fn an_endpoint_can_be_found_through_the_dht_alone() -> Result<()> {
    use iroh::endpoint::presets;
    use iroh_mainline_address_lookup::DhtAddressLookup;

    const ALPN: &[u8] = b"tain/dht-test";
    // Publishing a record and having it settle on enough nodes to be readable
    // is not quick, and neither is bootstrapping two independent DHT nodes.
    const BUDGET: Duration = Duration::from_secs(90);

    let publisher = iroh::Endpoint::builder(presets::N0)
        .alpns(vec![ALPN.to_vec()])
        .dns_resolver(tain_core::dns_cache::resolver())
        .bind()
        .await?;
    let dht = DhtAddressLookup::builder()
        .secret_key(publisher.secret_key().clone())
        .build()
        .context("the publisher could not start a DHT node")?;
    publisher.address_lookup()?.add(dht);
    let publisher_id = publisher.id();

    // A relay to publish. Without one the record is empty and the lookup below
    // would be testing nothing.
    tokio::time::timeout(Duration::from_secs(30), publisher.online())
        .await
        .context("the publisher never came online, so it had no relay to publish")?;

    let accepting = tokio::spawn(async move {
        let incoming = publisher.accept().await.context("publisher endpoint closed")?;
        incoming.await.context("handshake failed")?;
        Ok::<_, anyhow::Error>(())
    });

    let finder = iroh::Endpoint::builder(presets::N0)
        .dns_resolver(tain_core::dns_cache::resolver())
        .clear_address_lookup()
        .bind()
        .await?;
    let finder_dht = DhtAddressLookup::builder()
        .secret_key(finder.secret_key().clone())
        .build()
        .context("the finder could not start a DHT node")?;
    finder.address_lookup()?.add(finder_dht);

    // # Retried, not asked once
    //
    // A lookup against a DHT that has not yet been told anything comes back
    // *empty*, not slow — the first version of this asked once, six seconds in,
    // got "all address lookup services produced no results", and read like the
    // network blocking DHT traffic. It was not: `can_this_machine_reach_the_dht`
    // bootstraps here in two seconds against a hundred-odd peers. The record
    // simply had not settled on the nodes that would later be asked for it.
    //
    // So this asks repeatedly until the budget runs out. That is also what
    // makes a failure meaningful: with the bootstrap test passing beside it,
    // the only thing left for this to be measuring is propagation.
    let started = std::time::Instant::now();
    let mut attempts = 0u32;
    let mut last: Option<String> = None;
    let conn = loop {
        if started.elapsed() >= BUDGET {
            anyhow::bail!(
                "the DHT never returned a record for this endpoint in {BUDGET:?} \
                 ({attempts} attempts); last answer: {}",
                last.as_deref().unwrap_or("none")
            );
        }
        attempts += 1;
        match finder.connect(iroh::EndpointAddr::new(publisher_id), ALPN).await {
            Ok(conn) => break conn,
            Err(e) => {
                last = Some(format!("{e:#}"));
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    };

    assert_eq!(conn.remote_id(), publisher_id, "found the wrong endpoint");
    eprintln!("found through the DHT in {:?} ({attempts} attempts)", started.elapsed());
    accepting.await.context("accept task panicked")??;
    Ok(())
}

/// Two machines on one network must find each other with nothing else running.
///
/// # What this proves
///
/// Both endpoints are built from `presets::Minimal`, which sets a crypto
/// provider and nothing else: no relay, no DNS address lookup, no n0
/// infrastructure of any kind. The only thing either one has is what
/// `discovery::attach` gives it. Neither is told the other's address.
///
/// So if a connection happens at all, it happened because one endpoint found
/// the other by multicast on the local network — there is no other route in
/// this configuration for it to have taken. That is the property worth pinning
/// down: for two machines in the same building, which is what a remote desktop
/// is mostly used for, none of the public internet has to be reachable.
///
/// The timeout doubles as the proof that mDNS and not the DHT answered: a DHT
/// lookup has to bootstrap against the public internet first and cannot return
/// inside this budget, and with no relay configured there is nothing else left.
#[tokio::test(flavor = "multi_thread")]
async fn two_endpoints_on_one_network_find_each_other_without_any_server() -> Result<()> {
    use iroh::endpoint::presets;

    let host = iroh::Endpoint::builder(presets::Minimal)
        .alpns(vec![b"tain/mdns-test".to_vec()])
        .bind()
        .await?;
    let up = tain_core::discovery::attach(&host);
    assert!(up.mdns, "multicast discovery did not start on this machine");
    // Not asserted: the DHT is a WAN mechanism and has no bearing on whether
    // two machines in one building can find each other. Printed because a run
    // where it did not start is worth knowing about.
    eprintln!("distributed (DHT) discovery started: {}", up.dht);
    let host_id = host.id();

    // The guest's ledger is the one that matters: finding is something the
    // dialling side does, and the host never looks itself up.
    let guest = iroh::Endpoint::builder(presets::Minimal).bind().await?;
    let finding = tain_core::discovery::attach(&guest);

    let accepting = tokio::spawn(async move {
        let incoming = host.accept().await.context("host endpoint closed")?;
        incoming.await.context("handshake failed")?;
        Ok::<_, anyhow::Error>(())
    });

    // Addressed by endpoint id alone — no relay url, no ip, nothing on file.
    //
    // Retried for the same reason `connect_reporting` retries: a lookup that
    // has not been told anything yet comes back *empty*, not slow, so one
    // attempt measures whether multicast had answered by that instant rather
    // than whether it answers. The n0 services are attached here too and fail
    // (there is no relay in this preset), which can end the aggregate lookup
    // before the multicast reply lands — most easily when the rest of this file
    // is running beside it, several mDNS responders deep on one machine.
    let started = std::time::Instant::now();
    let deadline = Duration::from_secs(30);
    let conn = loop {
        match guest.connect(iroh::EndpointAddr::new(host_id), b"tain/mdns-test").await {
            Ok(conn) => break conn,
            Err(e) if started.elapsed() >= deadline => anyhow::bail!(
                "an endpoint on this very machine was not found by multicast within \
                 {deadline:?} — either this network drops multicast, or local discovery is \
                 not attached: {e:#}"
            ),
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    };
    assert_eq!(conn.remote_id(), host_id, "connected to the wrong endpoint");

    // # Attribution, pinned down where it is deterministic
    //
    // There is no relay in this preset and the n0 lookups cannot work, so
    // multicast is the only service that could have produced this address.
    // That makes this the one place the branch can be asserted rather than
    // hoped for — every other test races several working services.
    assert_eq!(
        finding.found.branch(host_id),
        Some(tain_core::discovery::Branch::Lan),
        "multicast was the only thing that could have answered, so it must be credited"
    );
    accepting.await.context("accept task panicked")??;
    Ok(())
}

/// A host that can be reached directly must be *carried* directly.
///
/// # What this guards
///
/// Connecting no longer returns as soon as the transport answers: every iroh
/// connection begins on a relay and upgrades if it can, so the guest now waits
/// for the upgrade and refuses the session if it never comes. A relay is a
/// shared, rate-limited public server — ample for the handshake it exists to
/// carry, and a slideshow for video.
///
/// That wait is the kind of change that can go wrong in two opposite ways, and
/// this covers both. If the wait were broken, `connect_via` would hang here for
/// its full timeout and then refuse a connection to a host on this very
/// machine. If the *check* were broken — reading a relay as direct, or a
/// candidate iroh has not selected as one — it would pass while proving
/// nothing, so the path is asserted on afterwards as well.
///
/// Loopback for the same reason the test above uses it: it takes the relay out
/// of the picture entirely, so this runs on a machine that cannot reach one.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_can_be_reached_directly_is_carried_directly() -> Result<()> {
    let (server, _passcode, host_id) = start_host().await?;
    let addrs = loopback_addrs(&server);
    assert!(!addrs.is_empty(), "the host bound no dialable socket");
    let key = iroh::PublicKey::from_bytes(&host_id).context("host id")?;

    // # Why this budget is not tight
    //
    // A loopback address is passed as `known_addrs`, and those are no longer
    // raced against discovery — they are the last resort, tried only once
    // everything that could say where the host is *now* has come back with
    // nothing. So this test takes the slow path by construction: it waits for
    // the lookups to give up before the address it was handed is used at all.
    //
    // That is the point rather than an accident. A remembered address is not
    // evidence about the present, and preferring one was reported as breaking
    // connections that worked the moment it was deleted.
    //
    // What is still being timed is the punch, which is immediate to ourselves.
    // The budget covers discovery declining first.
    let guest = tokio::time::timeout(Duration::from_secs(40), GuestClient::connect_via(&key, None, &addrs))
        .await
        .context("connecting to a host on this machine took longer than the punch should")??;

    let path = guest.connection_path().context("connected with no path at all")?;
    assert!(
        path.is_direct(),
        "a host on this machine was carried over {} — the direct-path gate let a relay past",
        path.remote(),
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn guest_authenticates_and_receives_frames_over_iroh() -> Result<()> {
    let (server, passcode, host_id) = start_host().await?;

    let mut guest = GuestClient::connect_from_bytes(&host_id).await?;
    let (mut send, mut recv) = guest.authenticate(&passcode, Some("Test Guest")).await?;

    // First message must be SessionStart, carrying the host-assigned session id.
    let bytes = tokio::time::timeout(Duration::from_secs(30), read_stream_message(&mut recv))
        .await
        .context("timed out waiting for SessionStart")??
        .context("stream closed before SessionStart")?;

    let session_id = match bincode::deserialize::<Message>(&bytes)? {
        Message::SessionStart(ss) => {
            assert_eq!(ss.screen_info.width, 64);
            assert_eq!(ss.screen_info.height, 36);
            assert_eq!(
                ss.encoder_type,
                VideoCodec::H264.wire_name(),
                "host must advertise the encoder it actually runs"
            );
            assert_eq!(
                ss.host_name.as_deref(),
                Some("Test Host"),
                "the guest needs a name to file this connection under"
            );
            assert_ne!(ss.session_id, 0, "host must assign a real session id");
            ss.session_id
        }
        other => panic!("expected SessionStart, got {other:?}"),
    };

    // A master passcode gets control immediately, with no approval prompt.
    assert_eq!(server.sessions().grants(session_id), Some(ControlGrants::ALL));

    // Frames must arrive without the guest having sent anything since Auth,
    // and must decode with the codec the host said it was using.
    let mut decoder = H264Decoder::new()?;
    let mut decoded: Vec<Vec<u8>> = Vec::new();
    let mut frames = 0;
    tokio::time::timeout(Duration::from_secs(30), async {
        while frames < 3 {
            let bytes = read_stream_message(&mut recv).await?.context("stream closed")?;
            if let Message::Frame(f) = bincode::deserialize::<Message>(&bytes)? {
                assert_eq!((f.width, f.height), (64, 36));
                // Frames are compressed now, so the payload must be *smaller*
                // than the pixels it carries.
                assert!(
                    f.data.len() < 64 * 36 * 4,
                    "frame #{} was {} bytes, no smaller than raw",
                    f.sequence_number,
                    f.data.len()
                );
                if let Some(picture) = decoder.decode(&f.data).expect("frame failed to decode") {
                    assert_eq!((picture.width, picture.height), (64, 36));
                    decoded.push(picture.rgba);
                    frames += 1;
                }
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("timed out waiting for frames")??;

    // Decoding must reconstruct exactly the right number of pixels, and the
    // animated test pattern must actually differ between frames.
    assert!(decoded.iter().all(|f| f.len() == 64 * 36 * 4));
    assert!(decoded.windows(2).any(|w| w[0] != w[1]), "frames never changed");

    // Ping must be answered while the frame pump keeps running.
    write_stream_message(&mut send, &Message::Ping(99)).await?;
    let pong = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let bytes = read_stream_message(&mut recv).await?.context("stream closed")?;
            if let Message::Pong { echo: ts, .. } = bincode::deserialize::<Message>(&bytes)? {
                return Ok::<_, anyhow::Error>(ts);
            }
        }
    })
    .await
    .context("timed out waiting for Pong")??;
    assert_eq!(pong, 99);

    guest.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn a_connection_code_alone_is_enough_to_reach_the_host() -> Result<()> {
    // The whole point of the code format: the guest is given *only* the string,
    // never the endpoint id out of band.
    let server = Arc::new(HostServer::bind().await?.with_config(test_config()));
    let code_string = server.generate_master_passcode().await?;
    tokio::spawn({
        let server = Arc::clone(&server);
        async move { server.run().await }
    });

    let code = ConnectionCode::parse(&code_string)?;
    assert_eq!(
        code.endpoint_id,
        server.endpoint_id_bytes(),
        "code must address the host that issued it"
    );

    let mut guest = GuestClient::connect_with_code(&code, &[]).await?;
    let (_send, mut recv) = guest.authenticate(&code_string, None).await?;

    let bytes = tokio::time::timeout(Duration::from_secs(30), read_stream_message(&mut recv))
        .await
        .context("timed out waiting for SessionStart")??
        .context("stream closed before SessionStart")?;
    assert!(matches!(
        bincode::deserialize::<Message>(&bytes)?,
        Message::SessionStart(_)
    ));

    guest.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn a_code_for_a_different_host_is_rejected() -> Result<()> {
    // Same secret, wrong endpoint: the host must refuse rather than accept a
    // code that was never issued for it.
    let server = Arc::new(HostServer::bind().await?.with_config(test_config()));
    let real = ConnectionCode::parse(&server.generate_master_passcode().await?)?;
    tokio::spawn({
        let server = Arc::clone(&server);
        async move { server.run().await }
    });

    let forged = ConnectionCode::new(real.passcode_type, real.secret, [0xAAu8; 32]).encode();

    let mut guest = GuestClient::connect_from_bytes(&real.endpoint_id).await?;
    let (_send, mut recv) = guest.authenticate(&forged, None).await?;

    let bytes = tokio::time::timeout(Duration::from_secs(30), read_stream_message(&mut recv))
        .await
        .context("timed out waiting for rejection")??
        .context("stream closed without a reason")?;
    match bincode::deserialize::<Message>(&bytes)? {
        Message::SessionEnd(se) => assert_eq!(se.reason, "invalid_passcode"),
        other => panic!("expected rejection, got {other:?}"),
    }
    assert_eq!(server.sessions().count(), 0);

    guest.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn invalid_passcode_is_rejected_with_session_end() -> Result<()> {
    let (server, _passcode, host_id) = start_host().await?;

    let mut guest = GuestClient::connect_from_bytes(&host_id).await?;
    let (_send, mut recv) = guest.authenticate("MASTER-not-a-real-code", None).await?;

    let bytes = tokio::time::timeout(Duration::from_secs(30), read_stream_message(&mut recv))
        .await
        .context("timed out waiting for rejection")??
        .context("stream closed without a reason")?;

    match bincode::deserialize::<Message>(&bytes)? {
        Message::SessionEnd(se) => assert_eq!(se.reason, "invalid_passcode"),
        other => panic!("expected SessionEnd, got {other:?}"),
    }

    assert_eq!(server.sessions().count(), 0, "rejected auth must not create a session");

    guest.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}

/// The probe must answer honestly and, above all, silently: a saved-connection
/// card checking whether a host is up must not put a prompt on that host's
/// screen. Approval requests arriving here would make the whole feature unusable
/// — a card that refreshes every 45 seconds would be a doorbell.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn a_probe_answers_without_disturbing_the_host() -> Result<()> {
    use tain_core::network::{ProbeOutcome, Prober};

    let (approval_tx, mut approval_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = Arc::new(
        HostServer::bind()
            .await?
            .with_config(test_config())
            .with_approval_channel(approval_tx),
    );
    let guest_code = server.generate_guest_passcode().await?;
    let master_code = server.generate_master_passcode().await?;
    tokio::spawn({
        let server = Arc::clone(&server);
        async move { server.run().await }
    });

    let prober = Prober::bind().await?;
    let timeout = Duration::from_secs(20);

    // A valid guest code: reachable, current, and it says approval is needed.
    let code = ConnectionCode::parse(&guest_code)?;
    match prober.probe(&code, &[], timeout).await {
        ProbeOutcome::Valid { host_name, needs_approval } => {
            assert_eq!(host_name.as_deref(), Some("Test Host"));
            assert!(needs_approval, "a guest code must report that it will ask");
        }
        other => panic!("expected a valid probe, got {other:?}"),
    }

    // A master code is equally valid but would not wait for anyone.
    let master = ConnectionCode::parse(&master_code)?;
    match prober.probe(&master, &[], timeout).await {
        ProbeOutcome::Valid { needs_approval, .. } => {
            assert!(!needs_approval, "a master code connects without asking");
        }
        other => panic!("expected a valid probe, got {other:?}"),
    }

    // A code this host has never issued: reachable, but not current. That is
    // exactly the "the host regenerated while you were away" case, and it must
    // be distinguishable from the host being off.
    server.regenerate_passcode(tain_core::PasscodeType::Guest).await?;
    assert_eq!(prober.probe(&code, &[], timeout).await, ProbeOutcome::Stale);

    // And through all of it, nobody was asked anything.
    assert!(
        approval_rx.try_recv().is_err(),
        "probing raised an approval request — the host's user would be prompted \
         every time a saved card refreshed"
    );
    assert_eq!(server.sessions().count(), 0, "a probe must not leave a session behind");

    prober.shutdown().await;
    server.shutdown().await?;
    Ok(())
}

/// Bring up a host that requires approval, and connect a guest to it.
async fn start_host_with_guest(
    approval_timeout: Duration,
    client_name: Option<&str>,
) -> Result<(
    Arc<HostServer>,
    tokio::sync::mpsc::UnboundedReceiver<tain_core::ApprovalRequest>,
    GuestClient,
    iroh::endpoint::RecvStream,
)> {
    let (approval_tx, approval_rx) = tokio::sync::mpsc::unbounded_channel();
    let config = SessionConfig { approval_timeout, ..test_config() };
    let server = Arc::new(
        HostServer::bind()
            .await?
            .with_config(config)
            .with_approval_channel(approval_tx),
    );
    let guest_code = server.generate_guest_passcode().await?;
    let host_id = decode_endpoint_id(&server.endpoint_id())?;

    tokio::spawn({
        let server = Arc::clone(&server);
        async move { server.run().await }
    });

    let mut guest = GuestClient::connect_from_bytes(&host_id).await?;
    let (send, recv) = guest.authenticate(&guest_code, client_name).await?;
    // The send half is dropped by the caller's ignorance, so keep it alive by
    // leaking it into the guest — the guest never writes in these tests, but
    // dropping the stream would close it and race the host's own writes.
    std::mem::forget(send);

    Ok((server, approval_rx, guest, recv))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn a_guest_sees_nothing_until_the_host_allows_it() -> Result<()> {
    let (server, mut approval_rx, guest, mut recv) =
        start_host_with_guest(Duration::from_secs(30), Some("Test Guest")).await?;

    // The host raises an approval request carrying the guest's own name.
    let req = tokio::time::timeout(Duration::from_secs(30), approval_rx.recv())
        .await
        .context("timed out waiting for approval request")?
        .context("approval channel closed")?;
    let session_id = req.session_id;
    assert_eq!(req.label, "Test Guest");
    assert!(req.label_is_self_reported, "a guest-chosen name must be marked as such");
    assert_eq!(server.sessions().grants(session_id), Some(ControlGrants::NONE));

    // The guest is told to wait, and nothing else follows while it waits.
    match next_message(&mut recv).await.context("no message from host")? {
        Message::GuestApprovalRequest(_) => {}
        other => panic!("expected an approval request first, got {other:?}"),
    }
    let quiet =
        tokio::time::timeout(Duration::from_millis(800), read_stream_message(&mut recv)).await;
    assert!(quiet.is_err(), "the host streamed its screen before being allowed to");

    // Allowing the mouse alone must not hand over the keyboard as well.
    let partial = ControlGrants::new(true, false);
    req.approve_tx
        .send(ApprovalDecision::Allow { grants: partial, displays: tain_core::protocol::DisplayGrant::All })
        .expect("server dropped the approval responder");

    let mut saw_start = false;
    let mut announced = None;
    let mut saw_frame = false;
    for _ in 0..60 {
        match next_message(&mut recv).await {
            Some(Message::SessionStart(_)) => saw_start = true,
            Some(Message::ControlState(cs)) => announced = Some(cs.grants),
            Some(Message::Frame(_)) => {
                saw_frame = true;
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    assert!(saw_start, "an allowed guest should get SessionStart");
    assert!(saw_frame, "an allowed guest should get frames");
    assert_eq!(announced, Some(partial), "the guest must be told its permissions");
    assert_eq!(server.sessions().grants(session_id), Some(partial));

    guest.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn a_denied_guest_is_told_why_and_never_sees_a_frame() -> Result<()> {
    let (server, mut approval_rx, guest, mut recv) =
        start_host_with_guest(Duration::from_secs(30), None).await?;

    let req = tokio::time::timeout(Duration::from_secs(30), approval_rx.recv())
        .await
        .context("timed out waiting for approval request")?
        .context("approval channel closed")?;
    let session_id = req.session_id;
    // An anonymous guest still gets a usable label, just a generated one.
    assert_eq!(req.label, format!("Guest #{session_id}"));
    assert!(!req.label_is_self_reported);

    req.approve_tx.send(ApprovalDecision::Deny).expect("server dropped the responder");

    let mut ended = false;
    for _ in 0..10 {
        match next_message(&mut recv).await {
            Some(Message::SessionEnd(se)) => {
                assert!(!se.reason.is_empty(), "a refusal should say something");
                ended = true;
                break;
            }
            Some(Message::Frame(_)) => panic!("a denied guest must never see a frame"),
            Some(_) => {}
            None => break,
        }
    }
    assert!(ended, "a denied guest should be told, not just dropped");

    // And the session is cleared from the host's table.
    tokio::time::timeout(Duration::from_secs(10), async {
        while server.sessions().active_sessions().contains(&session_id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("a denied session was left in the table")?;

    guest.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn an_unanswered_guest_is_refused_rather_than_left_watching() -> Result<()> {
    // An unattended machine must not hand out its screen by default, so the
    // approval timeout has to deny rather than downgrade.
    let (server, mut approval_rx, guest, mut recv) =
        start_host_with_guest(Duration::from_millis(500), None).await?;

    let req = tokio::time::timeout(Duration::from_secs(30), approval_rx.recv())
        .await
        .context("timed out waiting for approval request")?
        .context("approval channel closed")?;
    // Deliberately never answer, and never drop the responder either — this is
    // the "nobody is at the keyboard" case, not the "UI went away" one.
    std::mem::forget(req.approve_tx);

    let mut ended = false;
    for _ in 0..10 {
        match next_message(&mut recv).await {
            Some(Message::SessionEnd(_)) => {
                ended = true;
                break;
            }
            Some(Message::Frame(_)) => panic!("a timed-out guest must never see a frame"),
            Some(_) => {}
            None => break,
        }
    }
    assert!(ended, "the guest should be refused once the prompt times out");

    guest.shutdown().await?;
    server.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn a_guest_that_gives_up_while_waiting_leaves_no_session_behind() -> Result<()> {
    // The approval wait is long enough for a guest to hang up mid-way. Every
    // write after that fails, so any early return between registering the
    // session and cleaning it up would strand a dead entry: the host would show
    // a pill for a guest that is gone, and the guest code would never
    // regenerate because the session count never reached zero.
    let (approval_tx, mut approval_rx) = tokio::sync::mpsc::unbounded_channel();
    let config = SessionConfig { approval_timeout: Duration::from_millis(600), ..test_config() };
    let server = Arc::new(
        HostServer::bind()
            .await?
            .with_config(config)
            .with_approval_channel(approval_tx),
    );
    let guest_code = server.generate_guest_passcode().await?;
    let host_id = decode_endpoint_id(&server.endpoint_id())?;
    tokio::spawn({
        let server = Arc::clone(&server);
        async move { server.run().await }
    });

    let mut guest = GuestClient::connect_from_bytes(&host_id).await?;
    let (_send, _recv) = guest.authenticate(&guest_code, None).await?;

    let req = tokio::time::timeout(Duration::from_secs(30), approval_rx.recv())
        .await
        .context("timed out waiting for approval request")?
        .context("approval channel closed")?;
    let session_id = req.session_id;
    assert!(server.sessions().active_sessions().contains(&session_id));

    // The guest walks away without waiting for an answer.
    guest.shutdown().await?;
    drop(req);

    tokio::time::timeout(Duration::from_secs(20), async {
        while server.sessions().active_sessions().contains(&session_id) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("the abandoned session was never cleaned up")?;

    assert_eq!(server.sessions().count(), 0);
    server.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn reconnecting_with_the_same_code_asks_again() -> Result<()> {
    // Approval is per connection, not per code. A guest that was allowed once
    // must not be able to walk back in unannounced.
    let (approval_tx, mut approval_rx) = tokio::sync::mpsc::unbounded_channel();
    let config =
        SessionConfig { auto_regen_guest_passcode: false, ..test_config() };
    let server = Arc::new(
        HostServer::bind()
            .await?
            .with_config(config)
            .with_approval_channel(approval_tx),
    );
    let guest_code = server.generate_guest_passcode().await?;
    let host_id = decode_endpoint_id(&server.endpoint_id())?;
    tokio::spawn({
        let server = Arc::clone(&server);
        async move { server.run().await }
    });

    let mut first_session = None;
    for attempt in 0..2 {
        let mut guest = GuestClient::connect_from_bytes(&host_id).await?;
        let (_send, mut recv) = guest.authenticate(&guest_code, None).await?;

        let req = tokio::time::timeout(Duration::from_secs(30), approval_rx.recv())
            .await
            .with_context(|| format!("no approval request on attempt {attempt}"))?
            .context("approval channel closed")?;

        match first_session {
            None => first_session = Some(req.session_id),
            Some(previous) => assert_ne!(
                previous, req.session_id,
                "a reconnect must be a new session, not a resumed one"
            ),
        }

        req.approve_tx.send(ApprovalDecision::Allow { grants: ControlGrants::ALL, displays: tain_core::protocol::DisplayGrant::All }).ok();

        // Drain until frames start, then hang up so the next attempt is clean.
        for _ in 0..60 {
            match next_message(&mut recv).await {
                Some(Message::Frame(_)) | None => break,
                Some(_) => {}
            }
        }
        guest.shutdown().await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    server.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn a_code_handed_out_yesterday_still_works_after_a_restart() -> Result<()> {
    // The whole point of saving the identity and the code together. Neither half
    // is sufficient: a restored passcode under a new keypair produces a string
    // that dials nowhere, and a restored keypair with a new passcode produces
    // one the host refuses. So this test keeps the code from the *first* run and
    // connects with it to the *second*.
    let first = HostServer::bind().await?.with_config(test_config());
    let code = first.generate_master_passcode().await?;
    let identity = first.identity_bytes();
    let parsed = ConnectionCode::parse(&code)?;
    first.shutdown().await?;
    drop(first);

    // A second process, as far as iroh is concerned.
    let second = Arc::new(
        HostServer::bind_with_identity(Some(identity)).await?.with_config(test_config()),
    );
    let restored = second.adopt_passcode(parsed.passcode_type, parsed.secret).await;
    assert_eq!(restored, code, "the code the user copied must be the code they still have");

    tokio::spawn({
        let server = Arc::clone(&second);
        async move { server.run().await }
    });

    let mut guest = GuestClient::connect_from_bytes(&parsed.endpoint_id).await?;
    let (_send, mut recv) = guest.authenticate(&code, Some("Returning Guest")).await?;

    let mut got_frame = false;
    for _ in 0..60 {
        match next_message(&mut recv).await {
            Some(Message::Frame(_)) => {
                got_frame = true;
                break;
            }
            Some(_) => {}
            None => break,
        }
    }
    assert!(got_frame, "yesterday's master code must stream today");

    guest.shutdown().await?;
    second.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires network access to the n0 relay"]
async fn a_fresh_identity_produces_a_different_endpoint() -> Result<()> {
    // Guards the assumption the whole feature rests on: iroh mints a new keypair
    // per bind unless told otherwise. If this ever stopped being true the
    // identity persistence would look like it worked while doing nothing.
    let a = HostServer::bind().await?;
    let b = HostServer::bind().await?;
    assert_ne!(a.endpoint_id_bytes(), b.endpoint_id_bytes());

    let reused = HostServer::bind_with_identity(Some(a.identity_bytes())).await?;
    assert_eq!(
        reused.endpoint_id_bytes(),
        a.endpoint_id_bytes(),
        "restoring the key must restore the address every code points at"
    );

    a.shutdown().await?;
    b.shutdown().await?;
    reused.shutdown().await?;
    Ok(())
}
