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
//! nothing to take offline. It is slower than a DNS query and it is not a
//! replacement for one, but it fails independently of everything else here,
//! which is the entire point of having it.
//!
//! # Attached after binding, on purpose
//!
//! `Builder::address_lookup` is the tidier way to do this, and it is the wrong
//! one: iroh calls `into_address_lookup` during `bind` and propagates the
//! error, so a machine where mDNS cannot open a multicast socket or the DHT
//! cannot bind its port would fail to start the application at all. These are
//! *extra* ways to find a peer. Not having one is worth a line in the log and
//! nothing more.

use iroh::Endpoint;
use tracing::*;

/// Which of the server-free lookup services came up.
///
/// Returned rather than only logged because "did local discovery start" is a
/// real question with a real answer, and a caller that wants to say so — a
/// test, or a diagnostics panel — should not have to scrape it out of a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attached {
    /// Multicast on the local network. The one that matters most.
    pub mdns: bool,
    /// The mainline DHT.
    pub dht: bool,
}

/// Attach the lookup services that depend on no particular server.
///
/// Call once, after `bind`. Failures are reported and skipped: every one of
/// these is an addition, and the endpoint works without any of them.
pub fn attach(endpoint: &Endpoint) -> Attached {
    let mut up = Attached::default();
    let Ok(services) = endpoint.address_lookup() else {
        debug!("Endpoint closed before local discovery could be attached");
        return up;
    };

    // # mDNS
    //
    // Advertises this endpoint id and its addresses to the local network, and
    // listens for others doing the same. Note that this is a local disclosure:
    // anyone on the same network can see that this machine is running Tain and
    // what its endpoint id is. That is not enough to connect — a passcode is
    // still required — but it is worth knowing on a network you do not own.
    match iroh_mdns_address_lookup::MdnsAddressLookup::builder().build(endpoint.id()) {
        Ok(mdns) => {
            services.add(mdns);
            up.mdns = true;
            info!("Local network discovery on (mDNS) — peers on this LAN need no relay");
        }
        // Reported at debug because the usual cause is a network that does not
        // carry multicast, which is normal and not a fault.
        Err(e) => debug!("No local network discovery ({e}) — a LAN peer will need a relay"),
    }

    // # Mainline DHT
    //
    // Publishes under this endpoint's own key, which is what makes the answer
    // trustworthy without a trusted server: the record is signed, and the key
    // it is signed with *is* the endpoint id being looked up.
    //
    // The default address filter is `relay_only`, so what goes into the public
    // DHT is which relay to find this endpoint at — not its IP addresses. That
    // is the right default and it is left alone: the relay is enough to
    // bootstrap a hole punch, and publishing a home address to a global
    // hash table to save one round trip is a poor trade.
    let dht = iroh_mainline_address_lookup::DhtAddressLookup::builder()
        .secret_key(endpoint.secret_key().clone())
        .build();
    match dht {
        Ok(dht) => {
            services.add(dht);
            up.dht = true;
            info!("Distributed discovery on (mainline DHT) — a peer can be found without DNS");
        }
        Err(e) => debug!("No distributed discovery ({e}) — falling back to DNS lookups"),
    }

    up
}
