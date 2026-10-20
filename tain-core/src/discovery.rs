//! Finding a peer without asking one particular server.
//!
//! # The problem with the default
//!
//! iroh's `presets::N0` sets up three address lookup services — a pkarr
//! publisher, a pkarr resolver over HTTPS, and DNS TXT lookups. That reads like
//! redundancy and is not: all three are pointed at the same host,
//! `dns.iroh.link`. Add the four relays, all under `relay.n0.iroh.link`, and
//! the whole of this application's ability to find anybody rests on five names
//! belonging to one organisation, resolved through whatever the local network
//! says DNS is.
//!
//! That is a poor arrangement for everyone, and not only where a network is
//! interfered with. It is a single operator's uptime, a single DNS answer that
//! can be wrong, and — for the case this application is most often used in, two
//! machines in the same building — a round trip through the public internet to
//! learn the address of a computer three metres away.
//!
//! # What this adds
//!
//! Two lookup services that need no central server at all. Both run alongside
//! the n0 ones rather than replacing them: iroh queries every service it has at
//! once and takes what comes back, so more of them is strictly better and a
//! service that is failing costs only the query it does not answer.
//!
//! **mDNS** answers "who is on this network" by multicast, with no DNS, no
//! relay and no internet. For two machines on the same LAN this is both the
//! fastest path and the only one that keeps working when everything else is
//! unreachable — and it is the case a remote desktop is used in most.
//!
//! **The mainline DHT** answers "where is this endpoint id" from the same
//! distributed hash table BitTorrent uses: millions of nodes, no operator, and
//! nothing to take offline. It is slower than a DNS query — measured at about
//! eight seconds against sub-second mDNS — and it is not a replacement for one,
//! but it fails independently of everything else here, which is the point.
//!
//! # Which one answered
//!
//! Worth knowing, and iroh does not say: [`Item::provenance`] exists on each
//! result, but nothing carries it out to where an application can see it —
//! `TransportAddrInfo` has the address and whether it is in use, and no record
//! of where it came from.
//!
//! So every service that resolves is wrapped in [`Noting`], which passes the
//! results through untouched and remembers which service produced the first one
//! for a given endpoint. That answer is what the session window shows in place
//! of a bare "direct": *how* this host was found is the interesting half, and
//! it is the half that says whether the local network, the DNS system, or the
//! DHT is the thing currently doing the work.
//!
//! # Built here rather than from the preset
//!
//! Wrapping the n0 services means adding them individually, which means this
//! module reproduces what `presets::N0` does rather than calling it. The
//! contents are small and documented — a publisher, two resolvers and the
//! default relay mode — and [`endpoint_builder`] is the one place that has to
//! track them.
//!
//! # Attached after binding, on purpose
//!
//! `Builder::address_lookup` is the tidier way to add these, and it is the
//! wrong one: iroh calls `into_address_lookup` during `bind` and propagates the
//! error, so a machine where mDNS cannot open a multicast socket or the DHT
//! cannot bind its port would fail to start the application at all. These are
//! *ways to find a peer*. Not having one is worth a line in the log and nothing
//! more.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use iroh::address_lookup::{
    AddressLookup, AddressLookupBuilder, EndpointData, Error as LookupError, Item,
};
use iroh::{Endpoint, EndpointId};
use n0_future::boxed::BoxStream;
use n0_future::StreamExt;
use tracing::*;

/// Which route located a host.
///
/// Ordered as they are tried in practice rather than alphabetically: the local
/// network first because it is fastest and needs nothing, then the DNS system,
/// then the DHT, which is the slowest and the one that keeps working when the
/// other two are blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Branch {
    /// Multicast on the local network. No DNS, no relay, no internet.
    Lan,
    /// The n0 lookup services — DNS TXT records, or the same data over HTTPS.
    Dns,
    /// The mainline DHT: no operator, and nothing to take offline.
    Dht,
}

impl Branch {
    /// Short enough to sit in a status row.
    pub fn label(self) -> &'static str {
        crate::i18n::text(match self {
            Self::Lan => "found.lan",
            Self::Dns => "found.dns",
            Self::Dht => "found.dht",
        })
    }

    /// What finding a host this way actually meant.
    pub fn describe(self) -> &'static str {
        crate::i18n::text(match self {
            Self::Lan => "found.lan_detail",
            Self::Dns => "found.dns_detail",
            Self::Dht => "found.dht_detail",
        })
    }
}

/// Which branch first answered for each endpoint.
///
/// Shared between the wrapped lookups and whoever wants to display the answer.
/// First answer wins and is never replaced: the services race, and the one that
/// got there first is the one that did the finding — a slower service arriving
/// afterwards with the same address did not.
#[derive(Debug, Clone, Default)]
pub struct Found(Arc<Mutex<HashMap<EndpointId, Branch>>>);

impl Found {
    fn note(&self, endpoint: EndpointId, branch: Branch) {
        let mut seen = self.0.lock().unwrap();
        if let std::collections::hash_map::Entry::Vacant(slot) = seen.entry(endpoint) {
            slot.insert(branch);
            debug!("{} located through {}", endpoint.fmt_short(), branch.label());
        }
    }

    /// How this endpoint was located, if anything has located it.
    pub fn branch(&self, endpoint: EndpointId) -> Option<Branch> {
        self.0.lock().unwrap().get(&endpoint).copied()
    }
}

/// An address lookup that records when it is the one with the answer.
///
/// Passes every result through untouched — this only watches.
#[derive(Debug)]
struct Noting<T> {
    inner: T,
    branch: Branch,
    found: Found,
}

/// Wrap a lookup so it records when it is the one with the answer.
///
/// A free function rather than a closure in [`attach`], because each service
/// has its own type — several of them opaque — and a closure would be fixed to
/// whichever one it was first handed.
fn noting<T: AddressLookup>(branch: Branch, found: &Found, inner: T) -> Noting<T> {
    Noting { inner, branch, found: found.clone() }
}

impl<T: AddressLookup> AddressLookup for Noting<T> {
    fn publish(&self, data: &EndpointData) {
        self.inner.publish(data);
    }

    fn resolve(&self, endpoint_id: EndpointId) -> Option<BoxStream<Result<Item, LookupError>>> {
        let stream = self.inner.resolve(endpoint_id)?;
        let (branch, found) = (self.branch, self.found.clone());
        Some(Box::pin(stream.map(move |result| {
            // An `Err` is a service reporting that it could not answer, which
            // is the opposite of having found something.
            if result.is_ok() {
                found.note(endpoint_id, branch);
            }
            result
        })))
    }
}

/// Addresses worth putting in a public directory.
///
/// # What this is for
///
/// Both directories this publishes to — the n0 lookup service and the mainline
/// DHT — default to publishing the relay and nothing else. That is a safe
/// default and it has a cost: a peer that reads the record learns only which
/// relay to meet at, and then has to hole punch. Where punching fails, there is
/// nothing else to try, and the session is refused.
///
/// Measured, on the session that prompted this: a tunnel on one machine and not
/// the other. DNS answered, gave the relay, the connection came up on it, and
/// no direct path ever opened. Waiting longer would not have helped — nor would
/// waiting for the DHT, which had the same relay and no addresses of its own to
/// offer. The address that would have worked was never published by anybody.
///
/// # Why not simply everything
///
/// `AddrFilter::unfiltered` also publishes private addresses, and those are the
/// worst of both: a `192.168.x` address is unreachable for the remote peer this
/// is published *for*, while telling anyone who reads the record how the local
/// network is laid out. The case they would serve — two machines in one
/// building — is answered by mDNS in under a second without publishing
/// anything anywhere.
///
/// So: the relay, and addresses that are actually routable from outside.
fn publishable() -> iroh::address_lookup::AddrFilter {
    iroh::address_lookup::AddrFilter::new(|addrs| {
        std::borrow::Cow::Owned(addrs.iter().filter(|addr| worth_publishing(addr)).cloned().collect())
    })
}

/// Whether a peer somewhere else on the internet could do anything with this.
fn worth_publishing(addr: &iroh::TransportAddr) -> bool {
    let ip = match addr {
        // The whole point of the record: where to meet to punch.
        iroh::TransportAddr::Relay(_) => return true,
        iroh::TransportAddr::Ip(socket) => socket.ip(),
        // A transport this build does not have. Publishing an address nothing
        // here can produce is not a decision worth guessing at.
        _ => return false,
    };
    match ip {
        std::net::IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            // Carrier-grade NAT (100.64/10) is as unreachable from outside as
            // RFC1918 and is not covered by `is_private`.
            let carrier_grade = a == 100 && (64..128).contains(&b);
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || carrier_grade)
        }
        std::net::IpAddr::V6(v6) => {
            // Unique local (fc00::/7) and link-local (fe80::/10) are the IPv6
            // equivalents of the ranges dropped above.
            let unique_local = (v6.octets()[0] & 0xfe) == 0xfc;
            let link_local = v6.octets()[0] == 0xfe && (v6.octets()[1] & 0xc0) == 0x80;
            !(v6.is_loopback() || v6.is_unspecified() || unique_local || link_local)
        }
    }
}

/// Relays of the user's own, and whether they join n0's or replace them.
///
/// # What a relay is for here, and what a list of them changes
///
/// A relay is where two endpoints meet to hole punch: every connection starts
/// on one, and a session that cannot get off it is refused (see `network`).
/// An endpoint keeps one connection open, to its *home* relay, and that is the
/// only relay anybody can reach it through — which is why a code carries that
/// one and not a list.
///
/// iroh chooses the home relay from its map by latency. So **beside** n0's,
/// a relay of the user's own is the home relay whenever it answers fastest and
/// the fallback whenever n0's cannot be reached at all — on a network that
/// blocks them, say. **Instead of** n0's, it is the only place this machine
/// can be found, and n0's relays are never contacted for it.
///
/// Neither changes how a guest dials. iroh connects to whatever relay an
/// address names, in its map or not, so a guest reaches a host on a private
/// relay with nothing configured — the code says where.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayChoice {
    pub custom: Vec<iroh::RelayUrl>,
    /// Use `custom` instead of n0's relays. Ignored when `custom` is empty:
    /// an endpoint with no relays at all can only be found on its own network,
    /// which is not something to fall into by emptying a list.
    pub only: bool,
}

impl RelayChoice {
    /// Everything that parses, with what does not set aside to report.
    pub fn from_entries(entries: &[String], only: bool) -> (Self, Vec<String>) {
        let mut custom = Vec::new();
        let mut rejected = Vec::new();
        for entry in entries {
            match parse_relay(entry) {
                Ok(url) if !custom.contains(&url) => custom.push(url),
                Ok(_) => {}
                Err(_) => rejected.push(entry.clone()),
            }
        }
        // Cleared rather than kept and ignored, so two choices that do the same
        // thing compare equal.
        let only = only && !custom.is_empty();
        (Self { custom, only }, rejected)
    }

    /// Whether this changes anything from iroh's own relays.
    pub fn is_default(&self) -> bool {
        self.custom.is_empty()
    }

    fn relay_mode(&self) -> iroh::RelayMode {
        let default = iroh::endpoint::default_relay_mode();
        if self.custom.is_empty() {
            return default;
        }
        if self.only {
            return iroh::RelayMode::custom(self.custom.iter().cloned());
        }
        let map = default.relay_map();
        for url in &self.custom {
            map.insert(url.clone(), std::sync::Arc::new(iroh::RelayConfig::from(url.clone())));
        }
        iroh::RelayMode::Custom(map)
    }
}

/// A relay address as somebody would type it.
///
/// `https://` is assumed when no scheme is given, because a bare host name is
/// what people copy out of a server's configuration, and iroh relays are
/// served over HTTPS.
pub fn parse_relay(entry: &str) -> anyhow::Result<iroh::RelayUrl> {
    let entry = entry.trim();
    if entry.is_empty() {
        anyhow::bail!("an empty relay address");
    }
    let url = if entry.contains("://") { entry.to_string() } else { format!("https://{entry}") };
    let parsed: iroh::RelayUrl =
        url.parse().map_err(|e| anyhow::anyhow!("not a relay address: {entry} ({e})"))?;
    if !matches!(parsed.scheme(), "https" | "http") || parsed.host_str().is_none_or(str::is_empty)
    {
        anyhow::bail!("not an http or https address: {entry}");
    }
    Ok(parsed)
}

/// The relays every endpoint is built with.
///
/// Set once at startup, before anything binds: an endpoint takes its relay map
/// when it is built, so a change here reaches only the endpoints built after
/// it — which is why the setting says it applies from the next start.
static RELAYS: std::sync::RwLock<RelayChoice> =
    std::sync::RwLock::new(RelayChoice { custom: Vec::new(), only: false });

/// Build every endpoint from now on with these relays.
pub fn use_relays(choice: RelayChoice) {
    if choice.is_default() {
        debug!("Relays: n0's");
    } else {
        let urls: Vec<String> = choice.custom.iter().map(|u| u.to_string()).collect();
        info!(
            "Relays: {} {}",
            urls.join(", "),
            if choice.only { "only" } else { "beside n0's" }
        );
    }
    *RELAYS.write().unwrap() = choice;
}

/// The relays endpoints are being built with, for saying when a setting
/// differs from what is in use.
pub fn relays_in_use() -> RelayChoice {
    RELAYS.read().unwrap().clone()
}

/// A builder configured the way every endpoint here wants to be.
///
/// This is `presets::N0` written out, because the services it installs have to
/// be wrapped individually to know which of them answered — see the module
/// documentation. The publisher is the only one added here: it never resolves,
/// so there is nothing to attribute, and unlike the rest it cannot fail in a
/// way worth surviving.
pub fn endpoint_builder() -> iroh::endpoint::Builder {
    iroh::Endpoint::builder(iroh::endpoint::presets::Minimal)
        // Publishes routable addresses as well as the relay — see
        // `publishable`. This is the directory the DNS branch reads, and in the
        // session that prompted the change it was the branch that answered.
        .address_lookup(
            iroh::address_lookup::PkarrPublisher::n0_dns().addr_filter(publishable()),
        )
        // n0's, or the user's own beside or instead of them. See `RelayChoice`.
        .relay_mode(relays_in_use().relay_mode())
        // Relay hostnames answered from what was resolved last time, so a link
        // that cannot reach DNS can still reach the relays. See `dns_cache`.
        .dns_resolver(super::dns_cache::resolver())
}

/// Which lookup services came up, and where their answers are recorded.
#[derive(Debug, Clone, Default)]
pub struct Attached {
    /// The n0 lookups: DNS records, and the same data over HTTPS.
    pub dns: bool,
    /// Multicast on the local network. The one that matters most.
    pub mdns: bool,
    /// The mainline DHT.
    pub dht: bool,
    /// Which branch located each endpoint. See [`Found::branch`].
    pub found: Found,
}

/// Bind an endpoint and give it every way of finding a peer.
///
/// The two halves are one operation and are exposed as one, because a builder
/// from [`endpoint_builder`] that is bound without [`attach`] has *no resolving
/// lookups at all* — only the publisher, which is set at build time. That
/// endpoint would look perfectly healthy and be unable to find anybody, and
/// nothing about the call site would show it.
pub async fn bind(builder: iroh::endpoint::Builder) -> anyhow::Result<(Endpoint, Attached)> {
    let endpoint = builder.bind().await?;
    let lookups = attach(&endpoint);
    Ok((endpoint, lookups))
}

/// Attach every address lookup, each one noting when it is the one that answers.
///
/// Call once, after `bind` — or use [`bind`], which does both and cannot be
/// half-done. Failures are reported and skipped: every one of these is a way to
/// find a peer, and the endpoint works without any of them.
pub fn attach(endpoint: &Endpoint) -> Attached {
    let mut up = Attached::default();
    let Ok(services) = endpoint.address_lookup() else {
        debug!("Endpoint closed before address lookup could be attached");
        return up;
    };
    // # The n0 services
    //
    // Both report as `Dns`: one reads a DNS TXT record and the other fetches
    // the same data over HTTPS, and from the outside they are the same route
    // through the same host — which is the fact worth showing.
    match iroh::address_lookup::DnsAddressLookup::n0_dns().into_address_lookup(endpoint) {
        Ok(dns) => {
            services.add(noting(Branch::Dns, &up.found, dns));
            up.dns = true;
        }
        Err(e) => warn!("No DNS address lookup ({e})"),
    }
    match iroh::address_lookup::PkarrResolver::n0_dns().into_address_lookup(endpoint) {
        Ok(pkarr) => services.add(noting(Branch::Dns, &up.found, pkarr)),
        Err(e) => warn!("No HTTPS address lookup ({e})"),
    }

    // # mDNS
    //
    // Advertises this endpoint id and its addresses to the local network, and
    // listens for others doing the same. Note that this is a local disclosure:
    // anyone on the same network can see that this machine is running Tain and
    // what its endpoint id is. That is not enough to connect — a passcode is
    // still required — but it is worth knowing on a network you do not own.
    match iroh_mdns_address_lookup::MdnsAddressLookup::builder().build(endpoint.id()) {
        Ok(mdns) => {
            services.add(noting(Branch::Lan, &up.found, mdns));
            up.mdns = true;
            info!("Local network discovery on (mDNS) — peers on this LAN need no relay");
        }
        // # Loud, because this is the one that explains a LAN failure
        //
        // Reported at debug once, on the reasoning that a network which does
        // not carry multicast is normal and not a fault. True of the network
        // and wrong for the log: a machine that cannot start mDNS cannot find a
        // peer on its own network at all, which is exactly the symptom people
        // report — two PCs on one LAN, neither able to place the other — and
        // the one line that would have explained it was invisible at the level
        // anybody runs.
        //
        // On Windows this fails for reasons that *are* faults and are fixable:
        // the firewall blocking UDP 5353, or another program holding the port.
        Err(e) => warn!(
            "No local network discovery ({e}) — this machine cannot find a peer on its own \
             LAN and will need a relay or the DHT for every connection"
        ),
    }

    // # Mainline DHT
    //
    // Publishes under this endpoint's own key, which is what makes the answer
    // trustworthy without a trusted server: the record is signed, and the key
    // it is signed with *is* the endpoint id being looked up.
    //
    // The crate's default filter is `relay_only`, which publishes the relay and
    // nothing else. That is replaced with `publishable` so a peer reading this
    // record gets somewhere to dial as well as somewhere to meet — see that
    // function for the session that made the difference matter.
    let dht = iroh_mainline_address_lookup::DhtAddressLookup::builder()
        .secret_key(endpoint.secret_key().clone())
        .addr_filter(publishable())
        .build();
    match dht {
        Ok(dht) => {
            services.add(noting(Branch::Dht, &up.found, dht));
            up.dht = true;
            info!("Distributed discovery on (mainline DHT) — a peer can be found without DNS");
        }
        // Also loud, and for the same reason: on a link where the relays are
        // filtered this is the lookup that still answers, so losing it is the
        // difference between connecting and not.
        Err(e) => warn!("No distributed discovery ({e}) — falling back to DNS lookups"),
    }

    up
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn ip(addr: &str) -> iroh::TransportAddr {
        iroh::TransportAddr::Ip(addr.parse::<SocketAddr>().expect("test address"))
    }

    /// Only addresses another machine could actually reach get published.
    ///
    /// This decides what leaves this machine and goes into a public directory
    /// under a long-lived identity, so it is worth being exact about. The two
    /// failure directions are not symmetric: publishing too little means a peer
    /// has nothing to dial and the session is refused, and publishing too much
    /// means telling a global hash table how someone's home network is laid
    /// out, for addresses that peer could never have used.
    #[test]
    fn only_addresses_a_stranger_could_reach_are_published() {
        for public in ["93.184.216.34:41641", "8.8.8.8:1234", "[2606:2800:220:1::]:41641"] {
            assert!(worth_publishing(&ip(public)), "{public} is reachable and should be published");
        }

        for private in [
            "192.168.0.10:61369",  // RFC1918, and what mDNS already covers
            "10.1.2.3:41641",      // RFC1918
            "172.16.5.4:41641",    // RFC1918
            "127.0.0.1:41641",     // loopback
            "169.254.7.7:41641",   // link-local
            "100.100.1.1:41641",   // carrier-grade NAT, which is_private misses
            "0.0.0.0:41641",       // unspecified
            "[::1]:41641",         // loopback
            "[fd00::1]:41641",     // unique local
            "[fe80::1]:41641",     // link-local
        ] {
            assert!(
                !worth_publishing(&ip(private)),
                "{private} cannot be reached from outside and must not be published"
            );
        }
    }

    /// The relay is the one thing that must always be published.
    ///
    /// It is what a peer meets at in order to punch. Filtering it out would
    /// leave a record naming addresses and no way to arrange the punch that
    /// makes them usable.
    #[test]
    fn the_relay_is_always_published() {
        let relay = iroh::TransportAddr::Relay(
            "https://euc1-1.relay.n0.iroh.link./".parse().expect("relay url"),
        );
        assert!(worth_publishing(&relay));
    }

    /// A host name on its own is what people copy out of a server's config.
    #[test]
    fn a_relay_is_read_the_way_people_type_it() {
        let bare = parse_relay(" relay.example.org ").expect("a bare host name");
        assert_eq!(bare.scheme(), "https");
        assert_eq!(bare.host_str(), Some("relay.example.org"));

        let port = parse_relay("http://10.0.0.5:3340").expect("a scheme and port");
        assert_eq!(port.scheme(), "http");
        assert_eq!(port.port(), Some(3340));

        for junk in ["", "   ", "ftp://relay.example.org", "https://", "not a url at all"] {
            assert!(parse_relay(junk).is_err(), "accepted {junk:?}");
        }
    }

    /// What will not parse is reported rather than dropped without a word, and
    /// the same relay written twice is one relay.
    #[test]
    fn entries_that_do_not_parse_are_set_aside() {
        let entries = [
            "relay.example.org".to_string(),
            "https://relay.example.org/".to_string(),
            "ftp://nope".to_string(),
        ];
        let (choice, rejected) = RelayChoice::from_entries(&entries, true);
        assert_eq!(choice.custom.len(), 1);
        assert_eq!(rejected, vec!["ftp://nope".to_string()]);
    }

    /// Beside n0's, instead of them, or n0's alone when there is nothing to
    /// add — including when "only these" is ticked over an empty list.
    #[test]
    fn the_relay_map_is_what_was_chosen() {
        let ours = parse_relay("relay.example.org").unwrap();
        let n0 = iroh::endpoint::default_relay_mode().relay_map();
        assert!(!n0.is_empty(), "the comparison needs n0's relays to exist");

        let none = RelayChoice { custom: Vec::new(), only: true };
        assert_eq!(none.relay_mode(), iroh::endpoint::default_relay_mode());

        let beside = RelayChoice { custom: vec![ours.clone()], only: false }.relay_mode();
        let beside = beside.relay_map();
        assert!(beside.contains(&ours));
        assert_eq!(beside.len(), n0.len() + 1);

        let instead = RelayChoice { custom: vec![ours.clone()], only: true }.relay_mode();
        let instead = instead.relay_map();
        assert!(instead.contains(&ours));
        assert_eq!(instead.len(), 1);
    }
}
