//! A DNS cache in front of `getaddrinfo`, for both HTTP clients.
//!
//! # Why
//!
//! A skip used to wait about five seconds for the first byte of the new track
//! some of the time, and the five seconds were not the CDN. Measured from the
//! Pi (2026-10-27): with the Wi-Fi downlink saturated by a bulk download, as a
//! gapless prefetch saturates it, a fresh connection to the stream host read
//! `dns=5.030 tcp=5.063 tls=5.230 ttfb=5.278` — 5 s waiting for the name, then
//! a normal ~250 ms connection. A bare `getent ahosts` beside it took 5048 ms.
//! Idle, 47 lookups never passed 36 ms. That is glibc's resolver: a UDP query
//! or its answer is dropped in the full queue, nothing retransmits it, and
//! `resolv.conf`'s default `timeout:5` expires before the second try. The
//! daemon's slow probes (5063-5408 ms) all ran beside a prefetch download, and
//! two of them were edge cache HITS.
//!
//! A track load always resolves under exactly that load: the CDN pool drops
//! idle connections after 20 s (`cdn::POOL_IDLE_TIMEOUT`), so the probe opens a
//! new connection, and the prefetch of the very track being skipped to is
//! usually still downloading. The stream hosts' A/AAAA records live for 20 s,
//! so the router's cache is cold by then as well.
//!
//! # What this does
//!
//! It remembers the last answer per host. An answer younger than [`FRESH_FOR`]
//! is used as is. An older one, up to [`SERVE_STALE_FOR`], is ALSO used at
//! once, and a lookup runs in the background to replace it — so a connection
//! never waits on a lookup once the host has been resolved, and a lookup that
//! takes 5 s costs nothing but its own task. The prefetch's own connection
//! refreshes the answer, on a link that is not yet saturated, minutes before
//! the skip needs it.
//!
//! A stale Akamai answer is a safe thing to serve: an edge address keeps
//! answering for the hostnames it carries long after the mapping moves on. It
//! is bounded anyway — past [`SERVE_STALE_FOR`] (an idle night) the next
//! connection waits for a real lookup, and a failed refresh never extends an
//! answer's life. The lookup itself is the one reqwest's default resolver does
//! (`to_socket_addrs` on the blocking pool), so address order — IPv6 first on
//! the Pi, per RFC 6724 — is unchanged.
//!
//! What it cannot fix: the TCP and TLS packets of a new connection share the
//! same full queue. They retransmit on their own (about 1 s at worst, measured
//! ~250 ms), so they cost a little and never the 5 s.

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Within this, an answer is used without a lookup. The stream hosts' TTL.
const FRESH_FOR: Duration = Duration::from_secs(20);

/// Up to this, an older answer is still used at once while a lookup refreshes
/// it. Past it, a connection waits for the lookup.
const SERVE_STALE_FOR: Duration = Duration::from_secs(30 * 60);

/// A refresh still unanswered after this is presumed lost (its task died with
/// a runtime, say) and another may start. glibc gives up after 2 tries x 5 s
/// per nameserver, so a live one is done well inside it.
const REFRESH_GIVE_UP: Duration = Duration::from_secs(30);

/// A lookup slower than this is logged, so a stall is attributable next time.
const SLOW_LOOKUP: Duration = Duration::from_secs(1);

type Lookup =
    Arc<dyn Fn(String) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> + Send + Sync>;

struct Entry {
    addrs: Vec<SocketAddr>,
    resolved_at: Instant,
    refreshing_since: Option<Instant>,
}

#[derive(Clone)]
pub(crate) struct CachingResolver {
    lookup: Lookup,
    fresh_for: Duration,
    serve_stale_for: Duration,
    entries: Arc<Mutex<HashMap<String, Entry>>>,
}

/// The one resolver both clients share, so the API host and the CDN hosts are
/// each looked up once per process, not once per client.
pub(crate) fn shared() -> Arc<CachingResolver> {
    static SHARED: OnceLock<Arc<CachingResolver>> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| {
        Arc::new(CachingResolver::with_policy(
            Arc::new(|host| Box::pin(system_lookup(host))),
            FRESH_FOR,
            SERVE_STALE_FOR,
        ))
    }))
}

/// What reqwest's default `GaiResolver` does: `getaddrinfo` on the blocking pool.
async fn system_lookup(host: String) -> std::io::Result<Vec<SocketAddr>> {
    let started = Instant::now();
    let name = host.clone();
    let result = tokio::task::spawn_blocking(move || {
        (name.as_str(), 0).to_socket_addrs().map(Iterator::collect)
    })
    .await
    .map_err(std::io::Error::other)?;
    let took = started.elapsed();
    if took >= SLOW_LOOKUP {
        log::warn!("[dns] looking up {host} took {}ms", took.as_millis());
    }
    result
}

enum Plan {
    Use(Vec<SocketAddr>),
    UseAndRefresh(Vec<SocketAddr>),
    Wait,
}

impl CachingResolver {
    fn with_policy(lookup: Lookup, fresh_for: Duration, serve_stale_for: Duration) -> Self {
        Self {
            lookup,
            fresh_for,
            serve_stale_for,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn plan(&self, host: &str) -> Plan {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = entries.get_mut(host) else {
            return Plan::Wait;
        };
        let age = now.duration_since(entry.resolved_at);
        if age < self.fresh_for {
            return Plan::Use(entry.addrs.clone());
        }
        if age >= self.serve_stale_for {
            return Plan::Wait;
        }
        let refresh_in_flight = entry
            .refreshing_since
            .is_some_and(|since| now.duration_since(since) < REFRESH_GIVE_UP);
        if refresh_in_flight {
            return Plan::Use(entry.addrs.clone());
        }
        entry.refreshing_since = Some(now);
        Plan::UseAndRefresh(entry.addrs.clone())
    }

    async fn resolve_host(&self, host: String) -> std::io::Result<Vec<SocketAddr>> {
        match self.plan(&host) {
            Plan::Use(addrs) => Ok(addrs),
            Plan::UseAndRefresh(addrs) => {
                let lookup = (self.lookup)(host.clone());
                let entries = Arc::clone(&self.entries);
                tokio::spawn(async move {
                    let result = lookup.await;
                    store(&entries, &host, result);
                });
                Ok(addrs)
            }
            Plan::Wait => {
                let result = (self.lookup)(host.clone()).await;
                match result {
                    Ok(addrs) if !addrs.is_empty() => {
                        store(&self.entries, &host, Ok(addrs.clone()));
                        Ok(addrs)
                    }
                    Ok(_) => Err(no_addresses(&host)),
                    Err(err) => Err(err),
                }
            }
        }
    }
}

/// Record a lookup's answer. A failure (or an empty answer) keeps the old one
/// at its old age, so it cannot outlive `serve_stale_for`.
fn store(
    entries: &Mutex<HashMap<String, Entry>>,
    host: &str,
    result: std::io::Result<Vec<SocketAddr>>,
) {
    let mut entries = entries.lock().unwrap_or_else(|p| p.into_inner());
    match result {
        Ok(addrs) if !addrs.is_empty() => {
            entries.insert(
                host.to_string(),
                Entry {
                    addrs,
                    resolved_at: Instant::now(),
                    refreshing_since: None,
                },
            );
        }
        other => {
            let reason = match other {
                Err(err) => err.to_string(),
                Ok(_) => "no addresses".to_string(),
            };
            log::warn!("[dns] refreshing {host} failed ({reason}); keeping the last answer");
            if let Some(entry) = entries.get_mut(host) {
                entry.refreshing_since = None;
            }
        }
    }
}

fn no_addresses(host: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{host} resolved to no addresses"),
    )
}

impl Resolve for CachingResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_ascii_lowercase();
        let this = self.clone();
        Box::pin(async move {
            let addrs = this.resolve_host(host).await?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What one call of the scripted lookup does.
    #[derive(Clone)]
    enum Step {
        Answer(u8),
        Fail,
        /// Never answers: the 5 s glibc stall, made unbounded so a resolve that
        /// waits on it fails the test instead of passing slowly.
        Hang,
    }

    fn addr(last_octet: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, last_octet], 0))
    }

    struct Script {
        steps: Mutex<VecDeque<Step>>,
        calls: AtomicUsize,
    }

    fn resolver(
        steps: &[Step],
        fresh_for: Duration,
        stale: Duration,
    ) -> (CachingResolver, Arc<Script>) {
        let script = Arc::new(Script {
            steps: Mutex::new(steps.iter().cloned().collect()),
            calls: AtomicUsize::new(0),
        });
        let s = Arc::clone(&script);
        let lookup: Lookup = Arc::new(move |_host| {
            s.calls.fetch_add(1, Ordering::SeqCst);
            let step = s
                .steps
                .lock()
                .unwrap()
                .pop_front()
                .expect("an unscripted lookup");
            Box::pin(async move {
                match step {
                    Step::Answer(octet) => Ok(vec![addr(octet)]),
                    Step::Fail => Err(std::io::Error::other("scripted failure")),
                    Step::Hang => std::future::pending().await,
                }
            })
        });
        (
            CachingResolver::with_policy(lookup, fresh_for, stale),
            script,
        )
    }

    /// A resolve that must not wait on the network: bounded, so a regression
    /// that waits on a hung lookup fails here rather than hanging the suite.
    async fn resolve_now(r: &CachingResolver) -> std::io::Result<Vec<SocketAddr>> {
        let name = Name::from_str("streaming-qobuz-std.akamaized.net").unwrap();
        let resolving = r.resolve(name);
        match tokio::time::timeout(Duration::from_secs(2), resolving).await {
            Ok(result) => result
                .map(|addrs| addrs.collect())
                .map_err(std::io::Error::other),
            Err(_) => panic!("resolve waited on the lookup"),
        }
    }

    /// Resolve until the answer changes, the way a refresh lands: in the background.
    async fn resolve_until(r: &CachingResolver, want: SocketAddr) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if resolve_now(r).await.unwrap() == vec![want] {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the refreshed answer never arrived");
    }

    const HOUR: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn the_first_lookup_is_waited_for_and_then_remembered() {
        let (r, script) = resolver(&[Step::Answer(1)], HOUR, HOUR);
        assert_eq!(resolve_now(&r).await.unwrap(), vec![addr(1)]);
        assert_eq!(resolve_now(&r).await.unwrap(), vec![addr(1)]);
        assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_first_lookup_that_fails_is_an_error_and_is_not_remembered() {
        let (r, script) = resolver(&[Step::Fail, Step::Answer(1)], HOUR, HOUR);
        assert!(resolve_now(&r).await.is_err());
        assert_eq!(resolve_now(&r).await.unwrap(), vec![addr(1)]);
        assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    }

    /// The hardware bug: the lookup stalls (a dropped UDP query under a
    /// saturated downlink costs glibc 5 s) and the connection used to wait on it.
    #[tokio::test]
    async fn an_old_answer_is_used_at_once_while_the_lookup_stalls() {
        let (r, script) = resolver(&[Step::Answer(1), Step::Hang], Duration::ZERO, HOUR);
        resolve_now(&r).await.unwrap();
        assert_eq!(resolve_now(&r).await.unwrap(), vec![addr(1)]);
        tokio::task::yield_now().await;
        assert_eq!(
            script.calls.load(Ordering::SeqCst),
            2,
            "the old answer was used without refreshing it"
        );
    }

    #[tokio::test]
    async fn the_background_lookup_replaces_the_old_answer() {
        let (r, _) = resolver(
            &[Step::Answer(1), Step::Answer(2), Step::Hang],
            Duration::ZERO,
            HOUR,
        );
        resolve_now(&r).await.unwrap();
        resolve_until(&r, addr(2)).await;
    }

    #[tokio::test]
    async fn one_refresh_at_a_time() {
        let (r, script) = resolver(&[Step::Answer(1), Step::Hang], Duration::ZERO, HOUR);
        resolve_now(&r).await.unwrap();
        for _ in 0..5 {
            assert_eq!(resolve_now(&r).await.unwrap(), vec![addr(1)]);
        }
        tokio::task::yield_now().await;
        assert_eq!(script.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_old_answer_and_allows_another() {
        let (r, script) = resolver(
            &[Step::Answer(1), Step::Fail, Step::Answer(3), Step::Hang],
            Duration::ZERO,
            HOUR,
        );
        resolve_now(&r).await.unwrap();
        // Uses 1 and starts the refresh that fails; until it is recorded, 1 is
        // all there is. Then the next resolve may try again, and that one lands.
        resolve_until(&r, addr(3)).await;
        assert!(script.calls.load(Ordering::SeqCst) >= 3);
    }

    /// The bound on staleness: past it, the connection waits for a real lookup
    /// rather than dial an address from hours ago.
    #[tokio::test]
    async fn past_the_stale_limit_the_lookup_is_waited_for() {
        let (r, _) = resolver(
            &[Step::Answer(1), Step::Answer(2)],
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(resolve_now(&r).await.unwrap(), vec![addr(1)]);
        assert_eq!(resolve_now(&r).await.unwrap(), vec![addr(2)]);
    }

    #[tokio::test]
    async fn names_are_cached_case_insensitively() {
        let (r, script) = resolver(&[Step::Answer(1)], HOUR, HOUR);
        resolve_now(&r).await.unwrap();
        let upper = Name::from_str("STREAMING-QOBUZ-STD.akamaized.net").unwrap();
        let addrs: Vec<_> = r.resolve(upper).await.unwrap().collect();
        assert_eq!(addrs, vec![addr(1)]);
        assert_eq!(script.calls.load(Ordering::SeqCst), 1);
    }

    /// The real lookup, against a name that needs no network.
    #[tokio::test]
    async fn the_system_lookup_resolves_localhost() {
        let addrs = system_lookup("localhost".to_string()).await.unwrap();
        assert!(addrs.iter().all(|a| a.ip().is_loopback()), "{addrs:?}");
        assert!(!addrs.is_empty());
    }
}
