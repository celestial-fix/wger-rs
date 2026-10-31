//! Client identity behind the reverse proxy.
//!
//! Only a TCP peer listed in `TRUSTED_PROXIES` may supply `X-Forwarded-*` or
//! `X-Real-IP`; anything else is stripped before routing or proxying. This is
//! the same decision nginx makes implicitly by overwriting those headers, but
//! here the decision is explicit and testable.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::http::{header, HeaderMap, HeaderName, HeaderValue};

const X_FORWARDED_FOR: &str = "x-forwarded-for";
const X_FORWARDED_PROTO: &str = "x-forwarded-proto";
const X_FORWARDED_HOST: &str = "x-forwarded-host";
const X_REAL_IP: &str = "x-real-ip";

const XFF: HeaderName = HeaderName::from_static(X_FORWARDED_FOR);
const XFP: HeaderName = HeaderName::from_static(X_FORWARDED_PROTO);
const XFH: HeaderName = HeaderName::from_static(X_FORWARDED_HOST);
const XREAL: HeaderName = HeaderName::from_static(X_REAL_IP);

/// How often hostname entries in `TRUSTED_PROXIES` are resolved again.
pub const RESOLVE_INTERVAL: Duration = Duration::from_secs(30);

/// A single address or CIDR block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn single(addr: IpAddr) -> Self {
        let prefix = if addr.is_ipv4() { 32 } else { 128 };
        Self { addr, prefix }
    }

    /// Parse `10.0.0.0/8`, `192.0.2.7`, `::1`, or `fd00::/8`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if let Some((addr, prefix)) = spec.split_once('/') {
            let addr: IpAddr = addr
                .parse()
                .map_err(|_| format!("TRUSTED_PROXIES: {spec} is not an IP address"))?;
            let prefix: u8 = prefix
                .parse()
                .map_err(|_| format!("TRUSTED_PROXIES: {spec} has no numeric prefix"))?;
            let max = if addr.is_ipv4() { 32 } else { 128 };
            if prefix > max {
                return Err(format!(
                    "TRUSTED_PROXIES: {spec} has a prefix longer than /{max}"
                ));
            }
            Ok(Self { addr, prefix })
        } else {
            let addr: IpAddr = spec
                .parse()
                .map_err(|_| format!("TRUSTED_PROXIES: {spec} is not an IP or CIDR block"))?;
            Ok(Self::single(addr))
        }
    }

    pub fn contains(&self, candidate: IpAddr) -> bool {
        match (self.addr, candidate) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let net = u32::from(net);
                let ip = u32::from(ip);
                let mask = prefix_mask_v4(self.prefix);
                (net & mask) == (ip & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let net = u128::from(net);
                let ip = u128::from(ip);
                let mask = prefix_mask_v6(self.prefix);
                (net & mask) == (ip & mask)
            }
            _ => false,
        }
    }
}

fn prefix_mask_v4(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix.min(32)))
    }
}

fn prefix_mask_v6(prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix.min(128)))
    }
}

#[derive(Debug)]
struct Resolved {
    /// IPs from the last successful DNS lookup, keyed by the configured name.
    hostnames: RwLock<HashMap<String, Vec<IpNet>>>,
}

/// The fixed set of peers allowed to set forwarded headers.
#[derive(Debug)]
pub struct TrustedProxies {
    networks: Vec<IpNet>,
    hostnames: Vec<String>,
    resolved: Resolved,
}

impl TrustedProxies {
    /// Parse the comma-separated `TRUSTED_PROXIES` value.
    ///
    /// Hostnames are validated here but resolved only by [`Self::resolve`].
    pub fn from_specs(specs: &[String]) -> Result<Self, String> {
        let mut networks = Vec::new();
        let mut hostnames = Vec::new();
        for spec in specs {
            for item in spec.split(',') {
                let item = item.trim();
                if item.is_empty() {
                    continue;
                }
                if let Ok(net) = IpNet::parse(item) {
                    networks.push(net);
                } else if is_hostname(item) {
                    hostnames.push(item.to_ascii_lowercase());
                } else {
                    return Err(format!(
                        "TRUSTED_PROXIES: {item} is not an IP, CIDR block, or DNS hostname"
                    ));
                }
            }
        }
        Ok(Self {
            networks,
            hostnames,
            resolved: Resolved {
                hostnames: RwLock::new(HashMap::new()),
            },
        })
    }

    /// Trust exactly these addresses (tests and simple deployments).
    pub fn from_networks(networks: Vec<IpNet>) -> Self {
        Self {
            networks,
            hostnames: Vec::new(),
            resolved: Resolved {
                hostnames: RwLock::new(HashMap::new()),
            },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.networks.is_empty() && self.hostnames.is_empty()
    }

    pub fn is_trusted(&self, peer: IpAddr) -> bool {
        if self.networks.iter().any(|net| net.contains(peer)) {
            return true;
        }
        let resolved = self
            .resolved
            .hostnames
            .read()
            .unwrap_or_else(|e| e.into_inner());
        resolved.values().flatten().any(|net| net.contains(peer))
    }

    /// Resolve every hostname again. Failures keep the previous addresses, so
    /// a transient DNS outage does not silently start trusting the world.
    pub async fn resolve(&self) {
        for host in &self.hostnames {
            match tokio::net::lookup_host((host.as_str(), 0)).await {
                Ok(addrs) => {
                    let mut nets = Vec::new();
                    for addr in addrs {
                        let net = IpNet::single(addr.ip());
                        if !nets.contains(&net) {
                            nets.push(net);
                        }
                    }
                    let mut resolved = self
                        .resolved
                        .hostnames
                        .write()
                        .unwrap_or_else(|e| e.into_inner());
                    resolved.insert(host.clone(), nets);
                }
                Err(err) => {
                    tracing::warn!(host, error = %err, "TRUSTED_PROXIES hostname did not resolve");
                }
            }
        }
    }

    /// Resolve once at startup and then every [`RESOLVE_INTERVAL`].
    pub fn spawn_refresh(self: &Arc<Self>) {
        let proxies = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(RESOLVE_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                proxies.resolve().await;
            }
        });
    }
}

/// Hostname shape accepted in `TRUSTED_PROXIES`. Docker service names and FQDNs
/// both match; an IP is handled before this check.
fn is_hostname(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Rewrite the forwarded headers in place before any routing decision.
///
/// * Untrusted peer: the client cannot assert an identity. All incoming
///   `X-Forwarded-*`/`X-Real-IP` values are discarded, `X-Forwarded-Proto` is
///   `http` (this listener has no TLS), and both the forwarded chain and the
///   real IP are the peer address.
/// * Trusted peer: keep the scheme it supplied (default `http`), append the
///   peer to `X-Forwarded-For` (`$proxy_add_x_forwarded_for`), and use the
///   left-most address in the incoming chain as the client IP.
///
/// `X-Forwarded-Host` is always derived from the actual `Host` header, never
/// from a client-supplied `X-Forwarded-Host`.
pub fn normalize(headers: &mut HeaderMap, peer: Option<SocketAddr>, trusted: &TrustedProxies) {
    let host = headers.get(header::HOST).cloned();
    let incoming_for = header_text(headers.get(XFF));
    let incoming_proto = header_text(headers.get(XFP));

    headers.remove(XFF);
    headers.remove(XFP);
    headers.remove(XFH);
    headers.remove(XREAL);

    let trusted_peer = peer.is_some_and(|addr| trusted.is_trusted(addr.ip()));

    if trusted_peer {
        let peer_ip = peer.expect("trusted peer has an address").ip();
        let client_ip = incoming_for
            .as_deref()
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| peer_ip.to_string());
        let chain = match incoming_for.filter(|value| !value.is_empty()) {
            Some(value) => format!("{value}, {peer_ip}"),
            None => peer_ip.to_string(),
        };
        insert(headers, XFF, &chain);
        insert(headers, XREAL, &client_ip);
        insert(
            headers,
            XFP,
            incoming_proto
                .filter(|value| !value.is_empty())
                .as_deref()
                .unwrap_or("http"),
        );
    } else {
        if let Some(peer_ip) = peer.map(|addr| addr.ip()) {
            insert(headers, XFF, &peer_ip.to_string());
            insert(headers, XREAL, &peer_ip.to_string());
        }
        insert(headers, XFP, "http");
    }

    if let Some(host) = host {
        headers.insert(XFH, host);
    }
}

fn header_text(value: Option<&HeaderValue>) -> Option<String> {
    value
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn insert(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        map.insert(
            header::HOST,
            HeaderValue::from_static("fitness.example.test"),
        );
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    fn text(map: &HeaderMap, name: HeaderName) -> String {
        map.get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    fn peer(ip: &str) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), 4567)
    }

    #[test]
    fn untrusted_peer_cannot_spoof_forwarded_headers() {
        let trusted = TrustedProxies::from_networks(Vec::new());
        let mut map = headers(&[
            ("x-forwarded-for", "203.0.113.9"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "evil.example"),
            ("x-real-ip", "203.0.113.9"),
        ]);
        normalize(&mut map, Some(peer("198.51.100.4")), &trusted);

        assert_eq!(text(&map, XFF), "198.51.100.4");
        assert_eq!(text(&map, XREAL), "198.51.100.4");
        assert_eq!(text(&map, XFP), "http");
        assert_eq!(text(&map, XFH), "fitness.example.test");
    }

    #[test]
    fn trusted_peer_appends_to_the_chain_and_keeps_the_scheme() {
        let trusted = TrustedProxies::from_networks(vec![IpNet::parse("127.0.0.1").unwrap()]);
        let mut map = headers(&[
            ("x-forwarded-for", "203.0.113.9"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "upstream.example"),
            ("x-real-ip", "203.0.113.9"),
        ]);
        normalize(&mut map, Some(peer("127.0.0.1")), &trusted);

        assert_eq!(text(&map, XFF), "203.0.113.9, 127.0.0.1");
        assert_eq!(text(&map, XREAL), "203.0.113.9");
        assert_eq!(text(&map, XFP), "https");
        assert_eq!(text(&map, XFH), "fitness.example.test");
    }

    #[test]
    fn trusted_peer_without_forwarded_values_uses_its_own_address() {
        let trusted = TrustedProxies::from_networks(vec![IpNet::parse("10.0.0.0/8").unwrap()]);
        let mut map = headers(&[]);
        normalize(&mut map, Some(peer("10.1.2.3")), &trusted);

        assert_eq!(text(&map, XFF), "10.1.2.3");
        assert_eq!(text(&map, XREAL), "10.1.2.3");
        assert_eq!(text(&map, XFP), "http");
    }

    #[test]
    fn cidr_matching_is_family_and_prefix_aware() {
        let v4 = IpNet::parse("192.0.2.0/24").unwrap();
        assert!(v4.contains("192.0.2.55".parse().unwrap()));
        assert!(!v4.contains("192.0.3.1".parse().unwrap()));
        assert!(!v4.contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));

        let v6 = IpNet::parse("fd00::/8").unwrap();
        assert!(v6.contains("fd12:3456::1".parse().unwrap()));
        assert!(!v6.contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));

        assert!(IpNet::parse("0.0.0.0/0")
            .unwrap()
            .contains(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))));
        assert!(IpNet::parse("::/0")
            .unwrap()
            .contains(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn invalid_trusted_proxy_specs_are_rejected() {
        for spec in ["not a host!", "10.0.0.0/33", "::1/129", "-leading.example"] {
            assert!(
                TrustedProxies::from_specs(&[spec.to_string()]).is_err(),
                "{spec}"
            );
        }
        assert!(TrustedProxies::from_specs(&["localhost".into()]).is_ok());
    }

    #[tokio::test]
    async fn hostname_trusted_proxy_resolves_localhost() {
        let trusted = TrustedProxies::from_specs(&["localhost".to_string()]).unwrap();
        trusted.resolve().await;
        assert!(trusted.is_trusted(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }
}
