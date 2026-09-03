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
        match self {
            Self::Lan => "LAN",
            Self::Dns => "DNS",
            Self::Dht => "DHT",
        }
    }

    /// What finding a host this way actually meant.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Lan => {
                "Found on this local network by multicast — no relay, no DNS and no internet \
                 were involved in locating this host."
            }
            Self::Dns => {
                "Found through the public lookup service, either as a DNS record or the same \
                 data fetched over HTTPS."
            }
            Self::Dht => {
                "Found through the mainline DHT — the distributed table BitTorrent uses, which \
                 has no operator and keeps working where DNS is blocked."
            }
        }
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
        .relay_mode(iroh::endpoint::default_relay_mode())
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
        Err(e) => debug!("No distributed discovery ({e}) — falling back to DNS lookups"),
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
}
