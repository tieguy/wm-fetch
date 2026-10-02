//! SSRF guard, ported from SP42 (`sp42-fetch/src/resolver.rs`) and
//! wikiactive's `ssrf_guard`: validate the *resolved* address, not the URL
//! string.

use std::net::IpAddr;
use std::sync::Arc;

use ip_network::{Ipv4Network, Ipv6Network};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Whether an address is safe to connect to from untrusted input —
/// globally routable, not in any reserved/internal range (loopback, RFC1918
/// private, link-local incl. the `169.254.0.0/16` cloud-metadata range,
/// CGNAT, IPv6 ULA/link-local, unspecified, …).
#[must_use]
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => Ipv4Network::new(v4, 32).is_ok_and(|net| net.is_global()),
        // An IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) reaches the embedded
        // IPv4 host, but `Ipv6Network::is_global()` treats the whole
        // `::ffff/96` block as global — unwrap and classify the embedded IPv4
        // instead, or a mapped loopback/metadata address would slip through.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_public_ip(IpAddr::V4(v4)),
            None => Ipv6Network::new(v6, 128).is_ok_and(|net| net.is_global()),
        },
    }
}

/// Whether a URL's host is an IP *literal* in a non-public range. The
/// resolver guard only runs for DNS names, so literal hosts (initial URL or
/// redirect target) need this explicit check.
#[must_use]
pub fn host_is_blocked_literal(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !is_public_ip(ip.into()),
        Some(url::Host::Ipv6(ip)) => !is_public_ip(ip.into()),
        _ => false,
    }
}

/// Inner resolver delegating to the system resolver via tokio. Wrapped by
/// [`GuardedResolver`]; not used directly elsewhere.
struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs = tokio::net::lookup_host((host, 0)).await?;
            Ok(Box::new(addrs) as Addrs)
        })
    }
}

/// A reqwest DNS resolver that drops any resolved address that is not
/// globally routable, refusing the lookup when nothing survives. reqwest
/// runs it for every connection, including each (manually driven) redirect
/// hop — so an attacker-influenced hostname that resolves to an
/// internal/metadata address cannot be reached.
#[derive(Clone)]
pub struct GuardedResolver {
    inner: Arc<dyn Resolve>,
}

impl GuardedResolver {
    #[must_use]
    pub fn new(inner: Arc<dyn Resolve>) -> Self {
        Self { inner }
    }

    /// The production resolver: system lookups behind the guard.
    #[must_use]
    pub fn system() -> Self {
        Self::new(Arc::new(SystemResolver))
    }
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let resolved = inner.resolve(name).await?;
            let public: Vec<_> = resolved.filter(|addr| is_public_ip(addr.ip())).collect();
            if public.is_empty() {
                return Err("SSRF: host resolved only to non-public addresses".into());
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{is_public_ip, GuardedResolver};
    use reqwest::dns::{Addrs, Name, Resolve, Resolving};
    use std::net::{IpAddr, SocketAddr};
    use std::str::FromStr;
    use std::sync::Arc;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("valid ip")
    }

    #[test]
    fn blocks_reserved_and_internal_ranges() {
        for blocked in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(!is_public_ip(ip(blocked)), "{blocked} must be blocked");
        }
        for allowed in [
            "1.2.3.4",
            "93.184.216.34",
            "2606:2800:220:1:248:1893:25c8:1946",
        ] {
            assert!(is_public_ip(ip(allowed)), "{allowed} must be public");
        }
    }

    #[test]
    fn literal_hosts_classified() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert!(super::host_is_blocked_literal(&u("http://127.0.0.1/x")));
        assert!(super::host_is_blocked_literal(&u("http://10.0.0.5/x")));
        assert!(!super::host_is_blocked_literal(&u("http://example.org/x")));
    }

    /// An inner resolver that returns a fixed set of addresses regardless of name.
    struct StubResolver(Vec<SocketAddr>);

    impl Resolve for StubResolver {
        fn resolve(&self, _name: Name) -> Resolving {
            let addrs = self.0.clone();
            Box::pin(async move { Ok(Box::new(addrs.into_iter()) as Addrs) })
        }
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("valid socket addr")
    }

    #[tokio::test]
    async fn guard_keeps_only_public_resolved_addresses() {
        let inner = StubResolver(vec![addr("127.0.0.1:80"), addr("1.2.3.4:80")]);
        let guard = GuardedResolver::new(Arc::new(inner));
        let resolved: Vec<_> = guard
            .resolve(Name::from_str("mixed.test").expect("name"))
            .await
            .expect("at least one public address survives")
            .collect();
        assert_eq!(resolved, vec![addr("1.2.3.4:80")]);
    }

    #[tokio::test]
    async fn guard_refuses_when_every_resolved_address_is_private() {
        let inner = StubResolver(vec![addr("127.0.0.1:80"), addr("169.254.169.254:80")]);
        let guard = GuardedResolver::new(Arc::new(inner));
        let result = guard
            .resolve(Name::from_str("evil.test").expect("name"))
            .await;
        assert!(
            result.is_err(),
            "a host resolving only to private IPs must be refused"
        );
    }
}
