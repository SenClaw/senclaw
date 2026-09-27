//! Deterministic SSRF guard for URL-fetching tools.
//!
//! Port of sema-core `util/fetchSafety.ts`. A hit on loopback / link-local
//! (including cloud metadata 169.254.169.254) / private ranges / unspecified
//! addresses / local hostnames is treated as dangerous without consulting the
//! model. URL parse failures are treated as dangerous too.
//!
//! The `url` crate (WHATWG) already normalizes decimal/hex/octal/short IPv4
//! forms (`2130706433`, `0x7f000001`, `127.1`) to dotted decimal, so only the
//! extras are handled here: IPv4-mapped IPv6 (`::ffff:7f00:1`) and trailing-dot
//! FQDNs (`localhost.`).

use std::net::{Ipv4Addr, Ipv6Addr};

/// Returns `true` when the URL must not be fetched (SSRF risk or unparsable).
pub fn is_blocked_fetch_host(url: &str) -> bool {
    let parsed = match reqwest::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return true,
    };
    let hostname = match parsed.host_str() {
        Some(h) => h.to_ascii_lowercase(),
        None => return true,
    };

    // Strip IPv6 literal brackets and the trailing dot of an absolute FQDN.
    let host = hostname
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.');

    // Local hostnames / known cloud-metadata hostnames.
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if host == "metadata.google.internal" {
        return true;
    }

    // IPv4 literal (already normalized to dotted decimal by the url crate).
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return is_blocked_ipv4(v4);
    }

    // IPv6 literal: loopback / unspecified / ULA / link-local, and
    // IPv4-mapped addresses judged by the embedded IPv4.
    if let Ok(v6) = host.parse::<Ipv6Addr>() {
        if let Some(mapped) = v6.to_ipv4_mapped() {
            return is_blocked_ipv4(mapped);
        }
        if v6.is_loopback() || v6.is_unspecified() {
            return true;
        }
        let seg0 = v6.segments()[0];
        if seg0 & 0xfe00 == 0xfc00 {
            return true; // fc00::/7 ULA
        }
        if seg0 & 0xffc0 == 0xfe80 {
            return true; // fe80::/10 link-local
        }
        return false;
    }

    false
}

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, _, _] = ip.octets();
    match (a, b) {
        (0, _) => true,          // 0.0.0.0/8 unspecified
        (10, _) => true,         // 10.0.0.0/8 private
        (127, _) => true,        // 127.0.0.0/8 loopback
        (169, 254) => true,      // 169.254.0.0/16 link-local (incl. metadata)
        (172, 16..=31) => true,  // 172.16.0.0/12 private
        (192, 168) => true,      // 192.168.0.0/16 private
        (100, 64..=127) => true, // 100.64.0.0/10 CGNAT
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_loopback_and_private() {
        for url in [
            "http://127.0.0.1/x",
            "http://127.1.2.3:8080/",
            "http://10.0.0.5/",
            "http://172.16.0.1/",
            "http://172.31.255.255/",
            "http://192.168.1.1/",
            "http://100.64.0.1/",
            "http://0.0.0.0/",
        ] {
            assert!(is_blocked_fetch_host(url), "{url} should be blocked");
        }
    }

    #[test]
    fn blocks_link_local_and_metadata() {
        assert!(is_blocked_fetch_host(
            "http://169.254.169.254/latest/meta-data/"
        ));
        assert!(is_blocked_fetch_host(
            "http://metadata.google.internal/computeMetadata/v1/"
        ));
    }

    #[test]
    fn blocks_localhost_names() {
        assert!(is_blocked_fetch_host("http://localhost/"));
        assert!(is_blocked_fetch_host("http://localhost:3000/api"));
        assert!(is_blocked_fetch_host("http://foo.localhost/"));
        assert!(is_blocked_fetch_host("http://localhost./"));
    }

    #[test]
    fn blocks_ipv6_local_forms() {
        assert!(is_blocked_fetch_host("http://[::1]/"));
        assert!(is_blocked_fetch_host("http://[::]/"));
        assert!(is_blocked_fetch_host("http://[fc00::1]/"));
        assert!(is_blocked_fetch_host("http://[fd12:3456::1]/"));
        assert!(is_blocked_fetch_host("http://[fe80::1]/"));
        assert!(is_blocked_fetch_host("http://[::ffff:127.0.0.1]/"));
        assert!(is_blocked_fetch_host("http://[::ffff:7f00:1]/"));
        assert!(is_blocked_fetch_host("http://[::ffff:10.0.0.1]/"));
    }

    #[test]
    fn blocks_normalized_ipv4_shorthand() {
        // The url crate normalizes these to dotted-decimal loopback.
        assert!(is_blocked_fetch_host("http://2130706433/"));
        assert!(is_blocked_fetch_host("http://0x7f000001/"));
        assert!(is_blocked_fetch_host("http://127.1/"));
    }

    #[test]
    fn blocks_malformed_urls() {
        assert!(is_blocked_fetch_host("not a url"));
        assert!(is_blocked_fetch_host(""));
    }

    #[test]
    fn allows_public_hosts() {
        for url in [
            "https://example.com/",
            "https://api.github.com/repos",
            "http://8.8.8.8/",
            "http://[2606:4700::6810:84e5]/",
            "https://172.15.0.1/",
            "https://172.32.0.1/",
            "https://100.128.0.1/",
        ] {
            assert!(!is_blocked_fetch_host(url), "{url} should be allowed");
        }
    }
}
