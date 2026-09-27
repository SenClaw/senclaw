//! Which ports a sandbox may use.
//!
//! The network switch is coarse: on or off. This adds the case in between —
//! **no general network, but these specific ports are open** — which is what
//! running an app inside a sandbox actually needs: serve on 8000, reach it from
//! a browser, and reach nothing else.
//!
//! Two directions, because they are different permissions:
//!
//! * `listen` — the sandbox may **bind** the port, and you can reach it at
//!   `127.0.0.1:<port>` from this machine. This is the "run my app in a
//!   sandbox" case.
//! * `connect` — the sandbox may make **outbound** connections to that remote
//!   port and no other. `connect: [443]` is "may talk HTTPS, nothing else".
//!
//! # What each backend can actually enforce
//!
//! This differs enough that hiding it would be dishonest, so `note_for` returns
//! the difference and the UI and MCP both show it.
//!
//! | Backend | `listen` | `connect` |
//! |---|---|---|
//! | macOS Seatbelt | exact, per port | exact, per port |
//! | Docker | published to `127.0.0.1`, exact | **not filtered** — see below |
//! | Linux bubblewrap | works, but costs the network namespace | **not filtered** |
//!
//! Seatbelt is the precise one: its profile language filters both directions by
//! port, verified on a real machine before this module was written.
//!
//! Docker and bubblewrap cannot filter outbound by port without adding a
//! firewall or a proxy inside the sandbox. Worse, on both of them **opening a
//! listening port costs the network isolation**: a container with
//! `--network none` cannot publish anything, and a bubblewrap sandbox with
//! `--unshare-net` has no route to the host. So on those two, asking for a
//! listening port grants a network — and that is reported rather than quietly
//! done.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Ports below this need privileges to bind and are refused.
const MIN_PORT: u16 = 1024;
/// Keeping the list short keeps a sandbox profile readable and bounded.
const MAX_RULES: usize = 16;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PortPolicy {
    /// Ports the sandbox may bind; reachable from this machine.
    #[serde(default)]
    pub listen: Vec<u16>,
    /// Remote ports the sandbox may connect out to.
    #[serde(default)]
    pub connect: Vec<u16>,
    /// Loopback ports the sandbox may dial on **this machine**. Empty means it
    /// may reach no local service at all — see `seatbelt_rules` for why that is
    /// the default rather than a consequence of the other two lists.
    #[serde(default)]
    pub loopback: Vec<u16>,
}

impl PortPolicy {
    pub fn is_empty(&self) -> bool {
        self.listen.is_empty() && self.connect.is_empty() && self.loopback.is_empty()
    }

    /// True when the policy needs a network to work at all — which is what
    /// forces docker and bubblewrap to give one up.
    pub fn wants_network(&self) -> bool {
        !self.is_empty()
    }
}

/// Clean and check a policy. Rejects rather than silently repairs, because a
/// port the user thought they opened but did not is the kind of thing found
/// much later, by confusion.
pub fn validate(listen: &[u16], connect: &[u16], loopback: &[u16]) -> Result<PortPolicy> {
    // `privileged` applies to the listen list only. Binding a port below 1024
    // needs root; **connecting out** to one is ordinary traffic — 443 is HTTPS,
    // and refusing it would reject the single most useful rule anyone writes.
    let clean = |ps: &[u16], what: &str, privileged: bool| -> Result<Vec<u16>> {
        let mut out: Vec<u16> = Vec::new();
        for p in ps {
            if *p == 0 {
                return Err(anyhow!("port 0 is not a port ({what})"));
            }
            if privileged && *p < MIN_PORT {
                return Err(anyhow!(
                    "port {p} is below {MIN_PORT} ({what}); privileged ports need root and are not allowed here"
                ));
            }
            if !out.contains(p) {
                out.push(*p);
            }
        }
        if out.len() > MAX_RULES {
            return Err(anyhow!("too many {what} ports: {} (max {MAX_RULES})", out.len()));
        }
        out.sort_unstable();
        Ok(out)
    };
    Ok(PortPolicy {
        listen: clean(listen, "listen", true)?,
        connect: clean(connect, "connect", false)?,
        loopback: clean(loopback, "loopback", false)?,
    })
}

/// The macOS name-resolution socket. `getaddrinfo` does not send its own UDP
/// packets: it asks mDNSResponder over this Unix socket, so a profile that
/// denies `network*` breaks every hostname lookup no matter which ports are
/// open — measured, `connect:[53,443]` alone still could not fetch
/// `https://example.com`. Allowing the socket restores resolution while the
/// port filter keeps working (verified: with only `*:443` open, `http://` on
/// port 80 is still refused).
///
/// The trade-off is real and worth stating: a resolver is an exfiltration
/// channel (data encoded into hostnames). It is allowed only when the sandbox
/// already has some outbound permission, never for a network-off sandbox.
const MDNS_SOCKET: &str = "/private/var/run/mDNSResponder";

/// Seatbelt rules for the network section.
///
/// Emitted after a `(deny network*)`, so each `allow` re-opens exactly one
/// port; last matching rule wins in Seatbelt. Verified on macOS: with only
/// `*:53` allowed outbound, a connect to `:443` is refused, and a bind to a
/// port that was not listed is refused too.
///
/// # Loopback is denied even when the network is on
///
/// `*:443` includes `127.0.0.1:443`, and `network: true` includes every local
/// service on the machine — among them SenClaw's own REST API, which has no
/// authentication because its trust boundary is the loopback interface. That
/// combination undoes the sandbox: code that cannot read `~/.senclaw/oauth.json`
/// off the disk can ask the daemon for it over HTTP, and it can `POST
/// /api/sandbox/sandboxes` to build itself a second sandbox with `fsMode: open`
/// and the whole disk mounted. Both were demonstrated against a live daemon
/// before this deny existed.
///
/// So outbound to `localhost` is denied last (last match wins), and only the
/// ports in `loopback` are handed back. That list is also the mechanism for
/// per-site egress: Seatbelt cannot filter by host at all (its parser refuses
/// anything but `*` and `localhost`), so "only this website" has to be an
/// allowlisting proxy on loopback with direct egress left closed.
pub fn seatbelt_rules(policy: &PortPolicy, network: bool) -> String {
    let mut s = String::new();
    if network {
        // The coarse switch wins: full outbound, plus whatever may listen.
        s.push_str(";; network: enabled at sandbox level\n");
    } else if policy.is_empty() {
        s.push_str(";; ── network fully denied ──\n(deny network*)\n");
        return s;
    } else {
        s.push_str(";; ── network denied except the ports opened below ──\n(deny network*)\n");
    }

    for p in &policy.connect {
        s.push_str(&format!(
            "(allow network-outbound (remote ip \"*:{p}\"))\n"
        ));
    }
    for p in &policy.listen {
        // Binding alone is not enough to be reachable — the inbound connection
        // has to be permitted too, or the server listens and every client is
        // refused.
        s.push_str(&format!("(allow network-bind (local ip \"*:{p}\"))\n"));
        s.push_str(&format!("(allow network-inbound (local ip \"*:{p}\"))\n"));
    }

    // Hostname resolution, for sandboxes that may dial out at all.
    if network || !policy.connect.is_empty() {
        s.push_str(&format!(
            ";; name resolution (mDNSResponder), needed by any hostname lookup\n\
             (allow network-outbound (literal \"{MDNS_SOCKET}\"))\n"
        ));
    }

    // Loopback last, so it overrides the broad allows above. `listen`/inbound
    // rules are untouched — the host can still reach an app serving inside.
    s.push_str(";; ── this machine's own services: closed unless named ──\n");
    s.push_str("(deny network-outbound (remote ip \"localhost:*\"))\n");
    for p in &policy.loopback {
        s.push_str(&format!(
            "(allow network-outbound (remote ip \"localhost:{p}\"))\n"
        ));
    }
    s
}

/// `docker run` arguments publishing the listening ports.
///
/// Bound to `127.0.0.1` on purpose: `-p 8000:8000` alone listens on every
/// interface, which would put a sandboxed app on the LAN — the same mistake
/// this repo already made once by binding a Space App to `0.0.0.0`.
pub fn docker_publish_args(policy: &PortPolicy) -> Vec<String> {
    let mut a = Vec::new();
    for p in &policy.listen {
        a.push("-p".into());
        a.push(format!("127.0.0.1:{p}:{p}"));
    }
    a
}

/// What this backend will really do, in the user's terms. `None` when the
/// policy is empty or fully enforceable.
pub fn note_for(backend: &str, isolation: &str, policy: &PortPolicy) -> Option<String> {
    if policy.is_empty() {
        return None;
    }
    match (backend, isolation) {
        ("direct", "seatbelt") => None, // exact, nothing to warn about
        ("docker", _) => Some(
            "On docker, opening a port gives the container a network: published ports need one, \
             so outbound traffic is NOT limited to the `connect` list, and the host's own \
             services stay reachable through host.docker.internal regardless of `loopback`."
                .into(),
        ),
        ("direct", "bubblewrap") => Some(
            "On Linux, opening a listening port means the sandbox shares this machine's network \
             namespace, so outbound traffic is NOT limited to the `connect` list and this \
             machine's own services are reachable regardless of `loopback`."
                .into(),
        ),
        _ => Some(
            "This backend cannot enforce per-port rules; the ports are advisory only.".into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_ports_are_refused_with_the_reason() {
        let e = validate(&[80], &[], &[]).unwrap_err().to_string();
        assert!(e.contains("below 1024"), "got: {e}");
        assert!(validate(&[], &[443], &[]).is_ok(), "connecting OUT to 443 is fine");
        assert!(validate(&[443], &[], &[]).is_err(), "binding 443 is not");
        assert!(validate(&[], &[0], &[]).is_err(), "port 0 is never a port");
    }

    #[test]
    fn duplicates_collapse_and_the_list_is_bounded() {
        let p = validate(&[8000, 8000, 3000], &[], &[]).unwrap();
        assert_eq!(p.listen, vec![3000, 8000]);
        let many: Vec<u16> = (2000..2000 + MAX_RULES as u16 + 1).collect();
        assert!(validate(&many, &[], &[]).is_err());
    }

    #[test]
    fn an_empty_policy_denies_the_network_outright() {
        let s = seatbelt_rules(&PortPolicy::default(), false);
        assert!(s.contains("(deny network*)"));
        assert!(!s.contains("allow network"));
    }

    #[test]
    fn opened_ports_are_re_allowed_after_the_deny() {
        let p = validate(&[8000], &[443], &[]).unwrap();
        let s = seatbelt_rules(&p, false);
        let deny = s.find("(deny network*)").expect("must deny first");
        let out = s.find(r#"(allow network-outbound (remote ip "*:443"))"#).unwrap();
        let bind = s.find(r#"(allow network-bind (local ip "*:8000"))"#).unwrap();
        assert!(deny < out && deny < bind, "allows must follow the deny to win");
    }

    #[test]
    fn a_listening_port_also_allows_the_inbound_connection() {
        // Bind alone leaves a server that accepts nothing.
        let s = seatbelt_rules(&validate(&[8000], &[], &[]).unwrap(), false);
        assert!(s.contains(r#"(allow network-inbound (local ip "*:8000"))"#));
    }

    #[test]
    fn this_machines_own_services_are_denied_even_with_the_network_on() {
        // The escape this closes: `network: true` let sandboxed code call the
        // daemon's unauthenticated REST API on 127.0.0.1 — reading credentials
        // it could not read off the disk, and creating a second, unrestricted
        // sandbox through `POST /api/sandbox/sandboxes`.
        let s = seatbelt_rules(&PortPolicy::default(), true);
        assert!(s.contains(r#"(deny network-outbound (remote ip "localhost:*"))"#));
        let s2 = seatbelt_rules(&validate(&[], &[443], &[]).unwrap(), false);
        let allow_443 = s2.find(r#"(allow network-outbound (remote ip "*:443"))"#).unwrap();
        let deny_lo = s2.find(r#"(deny network-outbound (remote ip "localhost:*"))"#).unwrap();
        assert!(
            allow_443 < deny_lo,
            "`*:443` covers 127.0.0.1:443, so the loopback deny has to come after it to win"
        );
    }

    #[test]
    fn named_loopback_ports_are_handed_back_after_the_deny() {
        let p = validate(&[], &[], &[18788]).unwrap();
        let s = seatbelt_rules(&p, true);
        let deny = s.find(r#"(deny network-outbound (remote ip "localhost:*"))"#).unwrap();
        let allow = s.find(r#"(allow network-outbound (remote ip "localhost:18788"))"#).unwrap();
        assert!(deny < allow, "the re-allow must follow the deny to take effect");
    }

    #[test]
    fn a_served_port_stays_reachable_from_this_machine() {
        // The loopback deny is outbound-only; denying it inbound would break
        // the whole point of `listen`.
        let s = seatbelt_rules(&validate(&[8000], &[], &[]).unwrap(), false);
        assert!(s.contains(r#"(allow network-inbound (local ip "*:8000"))"#));
        assert!(!s.contains("(deny network-inbound"));
    }

    #[test]
    fn resolution_is_granted_only_to_sandboxes_that_may_dial_out() {
        // Measured on macOS: without this socket a hostname never resolves, so
        // `connect:[443]` cannot fetch anything by name.
        assert!(seatbelt_rules(&validate(&[], &[443], &[]).unwrap(), false).contains(MDNS_SOCKET));
        assert!(seatbelt_rules(&PortPolicy::default(), true).contains(MDNS_SOCKET));
        // Serving a port is not a reason to hand out a resolver.
        assert!(!seatbelt_rules(&validate(&[8000], &[], &[]).unwrap(), false).contains(MDNS_SOCKET));
        assert!(!seatbelt_rules(&PortPolicy::default(), false).contains(MDNS_SOCKET));
    }

    #[test]
    fn a_loopback_only_policy_still_counts_as_a_policy() {
        // Otherwise a proxy-only sandbox would be treated as "no ports asked
        // for" and docker/bubblewrap would not be told they need a network.
        let p = validate(&[], &[], &[8888]).unwrap();
        assert!(!p.is_empty() && p.wants_network());
    }

    #[test]
    fn the_coarse_switch_wins_over_connect_rules() {
        let s = seatbelt_rules(&validate(&[], &[443], &[]).unwrap(), true);
        assert!(!s.contains("(deny network*)"), "network:true must not be narrowed here");
    }

    #[test]
    fn published_ports_stay_on_loopback() {
        let a = docker_publish_args(&validate(&[8000, 9000], &[], &[]).unwrap()).join(" ");
        assert!(a.contains("-p 127.0.0.1:8000:8000"));
        assert!(a.contains("-p 127.0.0.1:9000:9000"));
        assert!(
            !a.contains("-p 8000:8000"),
            "an unqualified publish would expose a sandboxed app to the LAN"
        );
    }

    #[test]
    fn connect_ports_are_not_published() {
        // Outbound permission is not a reason to open a listening socket.
        assert!(docker_publish_args(&validate(&[], &[443], &[]).unwrap()).is_empty());
    }

    #[test]
    fn backends_that_cannot_enforce_say_so() {
        let p = validate(&[8000], &[443], &[]).unwrap();
        assert!(note_for("direct", "seatbelt", &p).is_none(), "seatbelt is exact");
        assert!(note_for("docker", "container", &p).unwrap().contains("NOT limited"));
        assert!(note_for("direct", "bubblewrap", &p).unwrap().contains("NOT limited"));
        assert!(note_for("docker", "container", &PortPolicy::default()).is_none());
    }
}
