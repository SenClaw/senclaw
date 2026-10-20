//! The LAN rule for SLink (internal name sen-link): what counts as a LAN
//! address, how a person's `host[:port]` is read, and the one HTTP client
//! every outbound call goes through.
//!
//! "LAN" means a *private network*, not a *trusted* one: loopback, RFC 1918,
//! link-local, unique-local IPv6 and the CGNAT block `100.64.0.0/10` (which is
//! what Tailscale uses). A public address is refused wherever it shows up — as
//! a literal, as the answer to a name, or as the source of an inbound
//! connection.
//!
//! The outbound half mirrors `datagen::url_fetch` with the condition turned
//! around: that client drops private addresses, this one keeps only them.
//! reqwest connects to the address the resolver returned, so a name cannot be
//! swapped for a public IP between the check and the connection; an IP
//! literal never reaches a resolver, so [`Endpoint::check`] judges it first.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Port a SLink listener uses unless `senLink.listenPort` says otherwise.
pub const DEFAULT_PORT: u16 = 18791;

/// Is `ip` an address SLink may talk to or accept connections from?
pub fn is_lan_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_lan_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_lan_v4(v4);
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                // fc00::/7 — unique local, including Tailscale's fd7a:115c:a1e0::/48
                || (first & 0xfe00) == 0xfc00
                // fe80::/10 — link-local
                || (first & 0xffc0) == 0xfe80
        }
    }
}

fn is_lan_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        // 100.64.0.0/10 — CGNAT, Tailscale's IPv4 block
        || (o[0] == 100 && (o[1] & 0xc0) == 0x40)
}

/// Where a peer is: an IP literal, or a name the system resolver answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    Ip(IpAddr),
    Name(String),
}

/// `host:port`, as a person typed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: Host,
    pub port: u16,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Host::Ip(IpAddr::V6(v6)) => write!(f, "[{v6}]:{}", self.port),
            Host::Ip(v4) => write!(f, "{v4}:{}", self.port),
            Host::Name(n) => write!(f, "{n}:{}", self.port),
        }
    }
}

impl Endpoint {
    /// `http://host:port/path`. Only plain HTTP: the LAN has no certificates.
    pub fn url(&self, path: &str) -> String {
        format!("http://{self}{path}")
    }

    /// An IP literal is judged here because it never reaches the resolver; a
    /// name is judged by [`LanOnly`] when it is resolved.
    pub fn check(&self) -> Result<(), String> {
        match self.host {
            Host::Ip(ip) if !is_lan_addr(ip) => Err(format!(
                "{ip} is not a LAN address (SLink only talks to private networks: 10.x, 172.16-31.x, 192.168.x, link-local, loopback, Tailscale)"
            )),
            _ => Ok(()),
        }
    }
}

/// Read `host[:port]`. Accepts an IPv4 or `[IPv6]` literal, or a name
/// (`mac-mini.local`, `nas.lan`, `mac-mini`); a pasted `http://…/` is
/// tolerated. A missing port becomes `default_port`.
pub fn parse_endpoint(input: &str, default_port: u16) -> Result<Endpoint, String> {
    let mut s = input.trim();
    if let Some((scheme, rest)) = s.split_once("://") {
        if !scheme.eq_ignore_ascii_case("http") {
            return Err(format!("{scheme}:// is not supported: SLink speaks plain http on the LAN"));
        }
        s = rest;
    }
    s = s.trim_end_matches('/');
    if s.is_empty() {
        return Err("the address is empty".into());
    }
    if s.contains('%') {
        return Err("IPv6 scope ids (fe80::1%en0) are not supported: use the IPv4 address or a name".into());
    }
    if s.chars().any(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '@' | '\\')) {
        return Err("give only host or host:port — no user, path or query".into());
    }

    let (host_part, port_part) = if let Some(rest) = s.strip_prefix('[') {
        let (inside, after) = rest.split_once(']').ok_or("missing ] after the IPv6 address")?;
        match after {
            "" => (inside, None),
            _ => (inside, Some(after.strip_prefix(':').ok_or("only :port may follow the ] of an IPv6 address")?)),
        }
    } else {
        match s.matches(':').count() {
            0 => (s, None),
            1 => {
                let (h, p) = s.split_once(':').expect("one colon");
                (h, Some(p))
            }
            // More than one colon and no brackets: a bare IPv6 literal.
            _ => {
                return match s.parse::<std::net::Ipv6Addr>() {
                    Ok(ip) => Ok(Endpoint { host: Host::Ip(IpAddr::V6(ip)), port: default_port }),
                    Err(_) => Err("IPv6 addresses with a port need brackets, e.g. [fd7a::1]:18791".into()),
                };
            }
        }
    };

    let port = match port_part {
        None => default_port,
        Some(p) => match p.parse::<u16>() {
            Ok(0) | Err(_) => return Err(format!("\"{p}\" is not a port (1-65535)")),
            Ok(n) => n,
        },
    };

    if let Ok(ip) = host_part.parse::<IpAddr>() {
        return Ok(Endpoint { host: Host::Ip(ip), port });
    }
    if s.starts_with('[') {
        return Err(format!("\"{host_part}\" is not an IPv6 address"));
    }
    Ok(Endpoint { host: Host::Name(valid_name(host_part)?), port })
}

fn valid_name(raw: &str) -> Result<String, String> {
    let name = raw.strip_suffix('.').unwrap_or(raw).to_ascii_lowercase();
    if name.is_empty() || name.len() > 253 {
        return Err(format!("\"{raw}\" is not a host name"));
    }
    // "127.1" and "1.2.3" are things some resolvers read as an IPv4 address.
    if name.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(format!("\"{raw}\" is not a valid IPv4 address"));
    }
    for label in name.split('.') {
        let ok = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            return Err(format!("\"{raw}\" is not a host name (letters, digits, - and _ only)"));
        }
    }
    Ok(name)
}

/// The addresses of `addrs` SLink may connect to.
fn keep_lan(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    addrs.into_iter().filter(|a| is_lan_addr(a.ip())).collect()
}

/// DNS for the outbound client: a name is usable only through its LAN addresses.
struct LanOnly;

impl Resolve for LanOnly {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let all: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            let lan = keep_lan(all);
            if lan.is_empty() {
                return Err(format!("{host} does not resolve to a LAN address").into());
            }
            Ok(Box::new(lan.into_iter()) as Addrs)
        })
    }
}

/// The client behind every outbound SLink call.
///
/// No proxy (a system proxy would resolve names itself and skip the LAN
/// check), no redirects (A2A has none, and a hop could leave the LAN), and
/// built here once: there is no "plain client" fallback to build instead.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(LanOnly))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("the LAN-only HTTP client builds (rustls backend)")
}

/// reqwest's top-level message hides the resolver's refusal one level down.
pub fn error_chain(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        out.push_str(": ");
        out.push_str(&s.to_string());
        src = s.source();
    }
    out
}

/// This machine's own addresses a peer could type, best first: IPv4, then
/// IPv6, loopback and link-local left out (neither is reachable from another
/// machine without a scope id).
pub fn local_addresses() -> Vec<IpAddr> {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        let ip = iface.ip();
        if iface.is_loopback() || !is_lan_addr(ip) {
            continue;
        }
        match ip {
            IpAddr::V4(a) if !a.is_link_local() => v4.push(ip),
            IpAddr::V6(a) if (a.segments()[0] & 0xffc0) != 0xfe80 => v6.push(ip),
            _ => {}
        }
    }
    v4.sort();
    v4.dedup();
    v6.sort();
    v6.dedup();
    v4.extend(v6);
    v4
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn private_networks_are_lan_and_the_internet_is_not() {
        for yes in [
            "10.0.0.1", "10.255.255.255", "172.16.0.1", "172.31.255.255", "192.168.1.20", "169.254.3.4", "127.0.0.1",
            "100.64.0.1", "100.101.102.103", "100.127.255.255", "::1", "fd00::1", "fc00::1", "fd7a:115c:a1e0::1", "fe80::1",
            "::ffff:192.168.1.5",
        ] {
            assert!(is_lan_addr(ip(yes)), "{yes} should be LAN");
        }
        for no in [
            "8.8.8.8", "172.32.0.1", "172.15.255.255", "100.63.255.255", "100.128.0.1", "192.169.0.1", "0.0.0.0", "255.255.255.255",
            "224.0.0.1", "2001:4860:4860::8888", "::", "::ffff:8.8.8.8", "::8.8.8.8", "ff02::1",
        ] {
            assert!(!is_lan_addr(ip(no)), "{no} should not be LAN");
        }
    }

    #[test]
    fn a_typed_address_is_read_as_host_and_port() {
        let p = |s: &str| parse_endpoint(s, DEFAULT_PORT);
        let ep = |h: Host, port| Endpoint { host: h, port };
        assert_eq!(p("192.168.1.20").unwrap(), ep(Host::Ip(ip("192.168.1.20")), 18791));
        assert_eq!(p("192.168.1.20:9000").unwrap(), ep(Host::Ip(ip("192.168.1.20")), 9000));
        assert_eq!(p("  100.101.102.103 ").unwrap(), ep(Host::Ip(ip("100.101.102.103")), 18791));
        assert_eq!(p("[fd7a:115c:a1e0::1]:18791").unwrap(), ep(Host::Ip(ip("fd7a:115c:a1e0::1")), 18791));
        assert_eq!(p("[::1]").unwrap(), ep(Host::Ip(ip("::1")), 18791));
        assert_eq!(p("fd00::5").unwrap(), ep(Host::Ip(ip("fd00::5")), 18791));
        assert_eq!(p("Mac-Mini.local").unwrap(), ep(Host::Name("mac-mini.local".into()), 18791));
        assert_eq!(p("mac-mini").unwrap(), ep(Host::Name("mac-mini".into()), 18791));
        assert_eq!(p("nas.lan.:7").unwrap(), ep(Host::Name("nas.lan".into()), 7));
        assert_eq!(p("macbook.tail1234.ts.net").unwrap(), ep(Host::Name("macbook.tail1234.ts.net".into()), 18791));
        assert_eq!(p("http://nas.lan:18791/").unwrap(), ep(Host::Name("nas.lan".into()), 18791));
        assert_eq!(p("HTTP://10.0.0.2/").unwrap(), ep(Host::Ip(ip("10.0.0.2")), 18791));
    }

    #[test]
    fn what_is_not_an_address_is_refused_with_a_reason() {
        let p = |s: &str| parse_endpoint(s, DEFAULT_PORT);
        for bad in [
            "", "   ", "user@host", "host/path", "host?x=1", "host#frag", "ftp://host", "https://host", "host:0", "host:70000", "host:abc",
            "host:", "[::1", "[::1]x", "[host]:1", "fe80::1%en0", "[fe80::1%en0]:1", "1.2.3", "127.1", "300.1.1.1", "-bad.local",
            "bad-.local", "a..b", "has space", "dot..", "fe80::zz", "1:2:3:4:5:6:7:8:9",
        ] {
            assert!(p(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(p("fe80::1%en0").unwrap_err().contains("scope"));
        assert!(p("https://h").unwrap_err().contains("plain http"));
    }

    #[test]
    fn an_endpoint_prints_the_way_it_is_dialled() {
        assert_eq!(parse_endpoint("[fd00::1]:5", 1).unwrap().to_string(), "[fd00::1]:5");
        assert_eq!(parse_endpoint("10.0.0.2", 7).unwrap().url("/a2a"), "http://10.0.0.2:7/a2a");
        assert_eq!(parse_endpoint("NAS.lan", 7).unwrap().url("/x"), "http://nas.lan:7/x");
    }

    #[test]
    fn a_public_ip_literal_is_refused_before_any_connection() {
        let public = parse_endpoint("8.8.8.8:18791", DEFAULT_PORT).unwrap();
        assert!(public.check().unwrap_err().contains("not a LAN address"));
        assert!(parse_endpoint("192.168.1.2", DEFAULT_PORT).unwrap().check().is_ok());
        // A name is not judged until it resolves.
        assert!(parse_endpoint("example.com", DEFAULT_PORT).unwrap().check().is_ok());
    }

    #[test]
    fn a_name_keeps_only_its_lan_addresses() {
        let sa = |s: &str| SocketAddr::new(ip(s), 0);
        let mixed = vec![sa("8.8.8.8"), sa("192.168.1.5"), sa("2001:4860:4860::8888"), sa("fd00::5")];
        assert_eq!(keep_lan(mixed), vec![sa("192.168.1.5"), sa("fd00::5")]);
        assert!(keep_lan(vec![sa("8.8.8.8"), sa("1.1.1.1")]).is_empty());
    }

    /// `localhost` answers 127.0.0.1 and/or ::1, both LAN: the resolver path works.
    #[tokio::test]
    async fn the_client_reaches_a_name_that_resolves_inside_the_lan() {
        let app = axum::Router::new().route("/ping", axum::routing::get(|| async { "pong" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let ep = parse_endpoint(&format!("localhost:{port}"), DEFAULT_PORT).unwrap();
        let body = client().get(ep.url("/ping")).send().await.unwrap().text().await.unwrap();
        assert_eq!(body, "pong");
    }

    /// A 302 to somewhere public is a response, not a hop.
    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let app = axum::Router::new().route(
            "/go",
            axum::routing::get(|| async { (axum::http::StatusCode::FOUND, [("location", "http://8.8.8.8/")], "") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let r = client().get(format!("http://127.0.0.1:{port}/go")).send().await.unwrap();
        assert_eq!(r.status(), 302);
    }

    #[test]
    fn this_machines_addresses_are_all_lan_and_none_is_loopback() {
        for a in local_addresses() {
            assert!(is_lan_addr(a) && !a.is_loopback(), "{a}");
            if let IpAddr::V6(v6) = a {
                assert_ne!(v6, Ipv6Addr::LOCALHOST);
            }
        }
    }
}
