//! Remembering where the relays are, so a censored link can still reach them.
//!
//! # The problem this exists for
//!
//! Every connection starts by resolving a relay hostname, and on a link where
//! DNS is interfered with that resolution mostly fails — after a three-second
//! timeout, every time, for every relay. The addresses themselves are perfectly
//! reachable; it is only the question "what address is this" that cannot get an
//! answer. A user behind such a link watches connections fail at the one step
//! that had nothing to do with the connection.
//!
//! # What it does
//!
//! Successful answers are written down, on disk, and used immediately the next
//! time the same name is asked for — no query, no timeout, no wait. The real
//! lookup still happens, in the background, and replaces what is stored if the
//! answer has changed. So a relay that moves is followed within one use, and a
//! relay that has not moved costs nothing to reach.
//!
//! That trade is the point: an address that turns out to be stale costs one
//! failed connection, and iroh already retries and has several relays to choose
//! from. Waiting for DNS costs three seconds *every* time, whether or not the
//! address was ever going to change.
//!
//! # What it deliberately does not cache
//!
//! TXT records. Those carry endpoint discovery data — where a *peer* is right
//! now — which is exactly the kind of thing that goes out of date, and serving
//! a stale one would send a connection to where a machine used to be. Relay
//! hostnames are infrastructure and change on the scale of months.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use iroh::dns::{BoxIter, DnsError, DnsResolver, Resolver, TxtRecordData, DNS_TIMEOUT};
use serde::{Deserialize, Serialize};
use tracing::*;

/// How old an answer may be before a use also triggers a refresh.
///
/// The refresh is in the background and does not delay anything, so this only
/// decides how often the network is asked — not how quickly a moved relay is
/// noticed, which is always "the next time it is used".
const REFRESH_AFTER: Duration = Duration::from_secs(30 * 60);

/// How long an unused answer is kept before being discarded as probably wrong.
///
/// Generous, because the whole point is to be useful on a link that may not
/// have reached DNS for days. A wrong address costs one failed connection; no
/// address at all costs every connection.
const FORGET_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// The one resolver every endpoint in this process shares.
///
/// Shared rather than built per endpoint so that the host, the prober and each
/// guest connection all benefit from what the others have already resolved —
/// they are all talking to the same handful of relays.
static RESOLVER: OnceLock<DnsResolver> = OnceLock::new();

/// Point the process's DNS at `cache_path`, if it has not been pointed yet.
///
/// Call once, before any endpoint is built. Calling it afterwards is not an
/// error and does nothing, which is the useful behaviour for a second call from
/// a test or a re-entered setup path.
pub fn install(cache_path: Option<PathBuf>) {
    if RESOLVER.get().is_some() {
        return;
    }
    let cache = Cache::load(cache_path);
    debug!("DNS cache holds {} name(s)", cache.len());
    let _ = RESOLVER.set(DnsResolver::custom(CachedDns::new(cache)));
}

/// The resolver to hand to `Endpoint::builder(..).dns_resolver(..)`.
///
/// Falls back to an unpersisted cache if [`install`] was never called, so a
/// caller that forgets still gets sharing within the process and simply loses
/// it at exit.
pub fn resolver() -> DnsResolver {
    RESOLVER
        .get_or_init(|| DnsResolver::custom(CachedDns::new(Cache::load(None))))
        .clone()
}

/// One name's last known addresses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Entry {
    #[serde(default)]
    v4: Vec<Ipv4Addr>,
    #[serde(default)]
    v6: Vec<Ipv6Addr>,
    /// When this was last confirmed, in seconds since the epoch.
    #[serde(default)]
    seen: u64,
}

impl Entry {
    fn age(&self) -> Duration {
        Duration::from_secs(now_unix().saturating_sub(self.seen))
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The names and addresses, and where they are written down.
#[derive(Debug)]
struct Cache {
    path: Option<PathBuf>,
    names: Mutex<HashMap<String, Entry>>,
}

impl Cache {
    fn load(path: Option<PathBuf>) -> Arc<Self> {
        let names = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str::<HashMap<String, Entry>>(&text).ok())
            .map(|mut names| {
                // Dropped on load rather than on use: an address nobody has
                // confirmed for a month is more likely to send a connection
                // somewhere wrong than to save one.
                names.retain(|_, entry| entry.age() < FORGET_AFTER);
                names
            })
            .unwrap_or_default();
        Arc::new(Self { path, names: Mutex::new(names) })
    }

    fn len(&self) -> usize {
        self.names.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn get(&self, host: &str) -> Option<Entry> {
        self.names.lock().unwrap_or_else(|e| e.into_inner()).get(host).cloned()
    }

    /// Record an answer, and say whether it differed from what was there.
    fn put(&self, host: &str, v4: Option<Vec<Ipv4Addr>>, v6: Option<Vec<Ipv6Addr>>) -> bool {
        let mut names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        let entry = names.entry(host.to_string()).or_default();
        let mut changed = false;
        if let Some(v4) = v4
            && entry.v4 != v4
        {
            entry.v4 = v4;
            changed = true;
        }
        if let Some(v6) = v6
            && entry.v6 != v6
        {
            entry.v6 = v6;
            changed = true;
        }
        entry.seen = now_unix();
        let snapshot = names.clone();
        drop(names);

        // Written every time rather than only on a change, because the
        // timestamp is what stops a good answer being forgotten after a month
        // of a link that cannot confirm it.
        self.save(&snapshot);
        changed
    }

    fn save(&self, names: &HashMap<String, Entry>) {
        let Some(path) = self.path.as_ref() else { return };
        let Ok(text) = serde_json::to_string_pretty(names) else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // A failure here costs the cache at the next start and nothing else, so
        // it is not worth interrupting a DNS lookup over.
        if let Err(e) = std::fs::write(path, text) {
            debug!("Could not write the DNS cache: {e}");
        }
    }
}

/// A resolver that answers from what it already knows, and checks afterwards.
#[derive(Debug)]
struct CachedDns {
    inner: Arc<dyn Resolver>,
    cache: Arc<Cache>,
}

impl CachedDns {
    fn new(cache: Arc<Cache>) -> Self {
        Self { inner: Arc::new(SystemDns::default()), cache }
    }

    fn with_inner(inner: Arc<dyn Resolver>, cache: Arc<Cache>) -> Self {
        Self { inner, cache }
    }

    /// Ask the network in the background and update what is stored.
    ///
    /// Nothing waits for this. It exists so that a name answered from the cache
    /// is still checked, which is what keeps a moved relay from being wrong
    /// forever — the check simply does not happen on the caller's time.
    fn refresh(&self, host: String, want_v6: bool) {
        // No runtime means no background work; the foreground answer stands.
        // This is the case in tests, and the cache is still correct without it.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let cache = Arc::clone(&self.cache);
        tokio::spawn(async move {
            let (v4, v6) = if want_v6 {
                (None, inner.lookup_ipv6(host.clone()).await.ok().map(|it| it.collect()))
            } else {
                (inner.lookup_ipv4(host.clone()).await.ok().map(|it| it.collect()), None)
            };
            if v4.as_ref().is_none_or(Vec::is_empty) && v6.as_ref().is_none_or(Vec::is_empty) {
                // The link is down or the name is blocked. What is stored is
                // the best thing anyone has, so it stays.
                return;
            }
            if cache.put(&host, v4, v6) {
                info!("{host} has moved — the address on file has been updated");
            }
        });
    }
}

/// `BoxFuture` from `n0-future`, written out so this crate need not depend on it.
type Boxed<T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;

impl Resolver for CachedDns {
    fn lookup_ipv4(&self, host: String) -> Boxed<Result<BoxIter<Ipv4Addr>, DnsError>> {
        if let Some(entry) = self.cache.get(&host)
            && !entry.v4.is_empty()
        {
            if entry.age() > REFRESH_AFTER {
                self.refresh(host.clone(), false);
            }
            debug!("{host} answered from the DNS cache ({} address(es))", entry.v4.len());
            let addrs = entry.v4;
            return Box::pin(async move { Ok(Box::new(addrs.into_iter()) as BoxIter<Ipv4Addr>) });
        }

        let inner = Arc::clone(&self.inner);
        let cache = Arc::clone(&self.cache);
        Box::pin(async move {
            let found: Vec<Ipv4Addr> = inner.lookup_ipv4(host.clone()).await?.collect();
            if !found.is_empty() {
                cache.put(&host, Some(found.clone()), None);
            }
            Ok(Box::new(found.into_iter()) as BoxIter<Ipv4Addr>)
        })
    }

    fn lookup_ipv6(&self, host: String) -> Boxed<Result<BoxIter<Ipv6Addr>, DnsError>> {
        if let Some(entry) = self.cache.get(&host)
            && !entry.v6.is_empty()
        {
            if entry.age() > REFRESH_AFTER {
                self.refresh(host.clone(), true);
            }
            let addrs = entry.v6;
            return Box::pin(async move { Ok(Box::new(addrs.into_iter()) as BoxIter<Ipv6Addr>) });
        }

        let inner = Arc::clone(&self.inner);
        let cache = Arc::clone(&self.cache);
        Box::pin(async move {
            let found: Vec<Ipv6Addr> = inner.lookup_ipv6(host.clone()).await?.collect();
            if !found.is_empty() {
                cache.put(&host, None, Some(found.clone()));
            }
            Ok(Box::new(found.into_iter()) as BoxIter<Ipv6Addr>)
        })
    }

    /// Straight through — see the module documentation for why these are not
    /// cached.
    fn lookup_txt(&self, host: String) -> Boxed<Result<BoxIter<TxtRecordData>, DnsError>> {
        self.inner.lookup_txt(host)
    }

    fn clear_cache(&self) {
        self.inner.clear_cache();
        // Deliberately not cleared. `clear_cache` is iroh asking its resolver to
        // forget what it learned this session, which is a different question
        // from whether an address written down last week is still the best
        // guess available on a link that cannot check.
    }

    fn reset(&self) -> Box<dyn Resolver> {
        // The sockets are rebound, the written-down addresses are not: a
        // network change is precisely when reaching DNS is least likely and
        // knowing where the relays were is most useful.
        Box::new(Self::with_inner(self.inner.reset().into(), Arc::clone(&self.cache)))
    }
}

/// The ordinary system resolver, wrapped so it can sit behind an `Arc`.
#[derive(Debug)]
struct SystemDns(DnsResolver);

impl Default for SystemDns {
    fn default() -> Self {
        Self(DnsResolver::new())
    }
}

impl Resolver for SystemDns {
    fn lookup_ipv4(&self, host: String) -> Boxed<Result<BoxIter<Ipv4Addr>, DnsError>> {
        let resolver = self.0.clone();
        Box::pin(async move {
            // The public API hands back `IpAddr` from a query it already knows
            // is A-only, so the narrowing below discards nothing.
            let found: Vec<Ipv4Addr> = resolver
                .lookup_ipv4(host, DNS_TIMEOUT)
                .await?
                .filter_map(|ip| match ip {
                    std::net::IpAddr::V4(v4) => Some(v4),
                    std::net::IpAddr::V6(_) => None,
                })
                .collect();
            Ok(Box::new(found.into_iter()) as BoxIter<Ipv4Addr>)
        })
    }

    fn lookup_ipv6(&self, host: String) -> Boxed<Result<BoxIter<Ipv6Addr>, DnsError>> {
        let resolver = self.0.clone();
        Box::pin(async move {
            let found: Vec<Ipv6Addr> = resolver
                .lookup_ipv6(host, DNS_TIMEOUT)
                .await?
                .filter_map(|ip| match ip {
                    std::net::IpAddr::V6(v6) => Some(v6),
                    std::net::IpAddr::V4(_) => None,
                })
                .collect();
            Ok(Box::new(found.into_iter()) as BoxIter<Ipv6Addr>)
        })
    }

    fn lookup_txt(&self, host: String) -> Boxed<Result<BoxIter<TxtRecordData>, DnsError>> {
        let resolver = self.0.clone();
        Box::pin(async move {
            let found: Vec<TxtRecordData> =
                resolver.lookup_txt(host, DNS_TIMEOUT).await?.collect();
            Ok(Box::new(found.into_iter()) as BoxIter<TxtRecordData>)
        })
    }

    fn clear_cache(&self) {
        self.0.clear_cache();
    }

    fn reset(&self) -> Box<dyn Resolver> {
        self.0.reset();
        Box::new(Self(self.0.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicBool, Ordering};

    /// What a [`FakeDns`] and its clones agree about.
    #[derive(Debug, Default)]
    struct Counters {
        asked: AtomicU32,
        refusing: AtomicBool,
        answer: Mutex<Vec<Ipv4Addr>>,
    }

    /// A resolver that answers a fixed address, or refuses, and counts asks.
    #[derive(Debug, Clone)]
    struct FakeDns(Arc<Counters>);

    impl FakeDns {
        fn answering(addr: [u8; 4]) -> Self {
            Self(Arc::new(Counters {
                answer: Mutex::new(vec![Ipv4Addr::from(addr)]),
                ..Default::default()
            }))
        }

        fn refuse(&self) {
            self.0.refusing.store(true, Ordering::Relaxed);
        }

        fn asked(&self) -> u32 {
            self.0.asked.load(Ordering::Relaxed)
        }
    }

    /// The one error shape these tests need. `DnsError` carries metadata from
    /// its derive, so it is built rather than named.
    fn refused() -> DnsError {
        DnsError::NoResponse { meta: Default::default() }
    }

    impl Resolver for FakeDns {
        fn lookup_ipv4(&self, _host: String) -> Boxed<Result<BoxIter<Ipv4Addr>, DnsError>> {
            self.0.asked.fetch_add(1, Ordering::Relaxed);
            if self.0.refusing.load(Ordering::Relaxed) {
                return Box::pin(async { Err(refused()) });
            }
            let answer = self.0.answer.lock().unwrap().clone();
            Box::pin(async move { Ok(Box::new(answer.into_iter()) as BoxIter<Ipv4Addr>) })
        }

        fn lookup_ipv6(&self, _host: String) -> Boxed<Result<BoxIter<Ipv6Addr>, DnsError>> {
            Box::pin(async { Err(refused()) })
        }

        fn lookup_txt(&self, _host: String) -> Boxed<Result<BoxIter<TxtRecordData>, DnsError>> {
            Box::pin(async { Err(refused()) })
        }

        fn clear_cache(&self) {}

        fn reset(&self) -> Box<dyn Resolver> {
            Box::new(self.clone())
        }
    }

    fn cached(inner: FakeDns, path: Option<PathBuf>) -> CachedDns {
        CachedDns::with_inner(Arc::new(inner), Cache::load(path))
    }

    async fn lookup(dns: &CachedDns, host: &str) -> Option<Vec<Ipv4Addr>> {
        dns.lookup_ipv4(host.into()).await.ok().map(|it| it.collect())
    }

    /// The second ask must not reach the network at all.
    ///
    /// This is the whole feature: on the link this was written for, reaching
    /// the network means a three-second timeout and then a failed connection.
    #[tokio::test]
    async fn a_name_already_known_is_answered_without_asking() {
        let inner = FakeDns::answering([1, 2, 3, 4]);
        let dns = cached(inner.clone(), None);

        assert_eq!(lookup(&dns, "relay.example").await.unwrap(), vec![Ipv4Addr::new(1, 2, 3, 4)]);
        assert_eq!(inner.asked(), 1);

        assert_eq!(lookup(&dns, "relay.example").await.unwrap(), vec![Ipv4Addr::new(1, 2, 3, 4)]);
        assert_eq!(
            inner.asked(),
            1,
            "the answer was already on file and the network was asked anyway"
        );
    }

    /// And once it is known, DNS failing stops mattering.
    #[tokio::test]
    async fn a_known_name_survives_dns_going_away() {
        let inner = FakeDns::answering([10, 0, 0, 1]);
        let dns = cached(inner.clone(), None);
        lookup(&dns, "relay.example").await.unwrap();

        // The link is censored from here on.
        inner.refuse();

        assert_eq!(
            lookup(&dns, "relay.example").await.unwrap(),
            vec![Ipv4Addr::new(10, 0, 0, 1)],
            "this is the case the cache exists for and it is the one that must not regress"
        );
    }

    /// A name nobody has ever resolved still fails, and says so.
    ///
    /// The cache is not allowed to invent an answer — a connection that fails
    /// for a nameable reason is better than one that goes somewhere arbitrary.
    #[tokio::test]
    async fn an_unknown_name_with_no_dns_still_fails() {
        let inner = FakeDns::answering([1, 1, 1, 1]);
        inner.refuse();
        let dns = cached(inner, None);

        assert!(lookup(&dns, "never-seen.example").await.is_none());
    }

    /// What was learned has to survive the application closing.
    ///
    /// An in-memory cache is empty exactly when it is needed most: at startup,
    /// which is when a user on a censored link is trying to connect and finding
    /// that they cannot.
    #[tokio::test]
    async fn what_was_resolved_is_still_known_after_a_restart() {
        let dir = std::env::temp_dir().join(format!("tain-dns-{}", std::process::id()));
        let path = dir.join("dns.json");
        let _ = std::fs::remove_file(&path);

        let first = FakeDns::answering([9, 9, 9, 9]);
        lookup(&cached(first, Some(path.clone())), "relay.example").await.unwrap();

        // A second process, with DNS unavailable to it.
        let second = FakeDns::answering([0, 0, 0, 0]);
        second.refuse();
        let restarted = cached(second.clone(), Some(path.clone()));

        assert_eq!(
            lookup(&restarted, "relay.example").await.unwrap(),
            vec![Ipv4Addr::new(9, 9, 9, 9)],
            "the file was written but not read back, so a cold start is still a cold start"
        );
        assert_eq!(second.asked(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An address that has been sitting unconfirmed for a month is dropped.
    #[tokio::test]
    async fn an_answer_nobody_has_confirmed_for_a_month_is_forgotten() {
        let dir = std::env::temp_dir().join(format!("tain-dns-old-{}", std::process::id()));
        let path = dir.join("dns.json");
        let _ = std::fs::create_dir_all(&dir);

        let stale = Entry {
            v4: vec![Ipv4Addr::new(5, 5, 5, 5)],
            v6: Vec::new(),
            seen: now_unix() - FORGET_AFTER.as_secs() - 1,
        };
        let names = HashMap::from([("relay.example".to_string(), stale)]);
        std::fs::write(&path, serde_json::to_string(&names).unwrap()).unwrap();

        let inner = FakeDns::answering([7, 7, 7, 7]);
        let dns = cached(inner, Some(path.clone()));
        assert_eq!(
            lookup(&dns, "relay.example").await.unwrap(),
            vec![Ipv4Addr::new(7, 7, 7, 7)],
            "a month-old address was served instead of asking"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
