//! The network scan: a person asks, and this walks a subnet looking for
//! SLinks. It never runs by itself — a TCP sweep of a network can set off an
//! IDS on a managed one — and it is bounded: at most [`MAX_HOSTS`] addresses,
//! [`CONCURRENCY`] at a time, one scan at once, IPv4 only (a /64 cannot be
//! walked; IPv6 peers are found by mDNS or typed).
//!
//! Each address gets a TCP connect, and only an open port gets an Agent Card
//! request. A host counts as a SLink when it answers with a SenClaw card that
//! carries the SLink extension; anything else is dropped without a trace.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::discovery::{Found, Source};
use super::lan::{self, Endpoint, Host};
use super::{probe, LinkState};

/// The most addresses one scan visits (a /22).
pub const MAX_HOSTS: usize = 1024;
const CONCURRENCY: usize = 128;
/// An address that does not answer a connect in this long is not a SLink.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
/// An open port that does not give its card in this long is not one either.
const CARD_TIMEOUT: Duration = Duration::from_millis(1500);

/// What to visit.
#[derive(Debug, Default, PartialEq)]
pub struct Targets {
    pub hosts: Vec<Ipv4Addr>,
    /// The subnets they come from, as `a.b.c.d/len`.
    pub ranges: Vec<String>,
    /// Things worth telling the person (a subnet that was narrowed or skipped).
    pub notes: Vec<String>,
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

/// The addresses a scan visits in `network/prefix`: all of them, minus the
/// network and broadcast addresses of a subnet that has them.
fn hosts_in(network: u32, prefix: u8) -> Vec<Ipv4Addr> {
    let last = network | !mask(prefix);
    let (from, to) = if prefix >= 31 { (network, last) } else { (network + 1, last - 1) };
    (from..=to).map(Ipv4Addr::from).collect()
}

/// `a.b.c.d/len` → the network address and the length. Host bits are ignored,
/// so `192.168.1.20/24` is the /24.
pub fn parse_cidr(s: &str) -> Result<(Ipv4Addr, u8), String> {
    let (ip, len) = s.trim().split_once('/').ok_or_else(|| format!("\"{s}\" is not a range: write it as a.b.c.d/len, e.g. 192.168.1.0/24"))?;
    let ip: Ipv4Addr = ip.parse().map_err(|_| format!("\"{ip}\" is not an IPv4 address (only IPv4 ranges can be scanned)"))?;
    let len: u8 = len.parse().ok().filter(|l| *l <= 32).ok_or_else(|| format!("\"{len}\" is not a prefix length (0-32)"))?;
    Ok((Ipv4Addr::from(u32::from(ip) & mask(len)), len))
}

/// The addresses of a range someone typed. It must sit inside the LAN and
/// hold at most [`MAX_HOSTS`] addresses. This machine's own addresses stay in:
/// a daemon never lists itself (the scan drops its own id), and a second one on
/// this machine is a fair thing to look for.
pub fn range_targets(cidr: &str) -> Result<Targets, String> {
    let (net, prefix) = parse_cidr(cidr)?;
    let start = u32::from(net);
    let end = start | !mask(prefix);
    if prefix < 22 {
        return Err(format!("{net}/{prefix} is too large: a scan visits at most {MAX_HOSTS} addresses, so a /22 or smaller"));
    }
    if !lan::is_lan_addr(IpAddr::V4(net)) || !lan::is_lan_addr(IpAddr::V4(Ipv4Addr::from(end))) {
        return Err(format!("{net}/{prefix} is not inside a private network; SLink only scans the LAN"));
    }
    Ok(Targets { hosts: hosts_in(start, prefix), ranges: vec![format!("{net}/{prefix}")], notes: Vec::new() })
}

/// The subnets this machine is on, from its `(address, prefix length)` pairs.
/// A subnet wider than a /22 is narrowed to the /24 around the address (and
/// the note says so), a point-to-point or single-address interface (Tailscale
/// is a /32) has nothing to walk, and the total stays under [`MAX_HOSTS`].
pub fn interface_targets(ifaces: &[(Ipv4Addr, u8)], own: &[Ipv4Addr]) -> Targets {
    let mut out = Targets::default();
    let mut seen: HashSet<Ipv4Addr> = HashSet::new();
    let mut done: HashSet<(u32, u8)> = HashSet::new();
    for &(ip, prefix) in ifaces {
        if ip.is_loopback() || ip.is_link_local() || !lan::is_lan_addr(IpAddr::V4(ip)) || prefix >= 31 || prefix == 0 {
            continue;
        }
        let eff = if prefix < 22 { 24 } else { prefix };
        let net = u32::from(ip) & mask(eff);
        if !done.insert((net, eff)) {
            continue;
        }
        let label = format!("{}/{eff}", Ipv4Addr::from(net));
        if eff != prefix {
            out.notes.push(format!("{ip}/{prefix} is wider than a /22, so only {label} is scanned"));
        }
        let fresh: Vec<Ipv4Addr> = hosts_in(net, eff).into_iter().filter(|h| !own.contains(h) && !seen.contains(h)).collect();
        if out.hosts.len() + fresh.len() > MAX_HOSTS {
            out.notes.push(format!("{label} was skipped: a scan visits at most {MAX_HOSTS} addresses"));
            continue;
        }
        seen.extend(fresh.iter().copied());
        out.hosts.extend(fresh);
        out.ranges.push(label);
    }
    out
}

/// This machine's IPv4 interfaces as `(address, prefix length)`, and every
/// IPv4 address it has (including loopback). Loopback and point-to-point
/// interfaces — a VPN, Tailscale — are not listed: a scan that nobody aimed
/// should stay on the networks this machine is plainly a member of, not sweep
/// the far end of a tunnel. A range typed by hand can still name one.
pub fn local_ipv4() -> (Vec<(Ipv4Addr, u8)>, Vec<Ipv4Addr>) {
    let mut ifaces = Vec::new();
    let mut own = Vec::new();
    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = &i.addr {
            own.push(v4.ip);
            if !i.is_loopback() && !i.is_p2p() {
                ifaces.push((v4.ip, u32::from(v4.netmask).count_ones() as u8));
            }
        }
    }
    (ifaces, own)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Running,
    Done,
    Canceled,
}

pub struct ScanJob {
    pub id: String,
    pub port: u16,
    pub ranges: Vec<String>,
    pub notes: Vec<String>,
    pub total: usize,
    scanned: AtomicUsize,
    state: Mutex<State>,
    found: Mutex<Vec<Found>>,
    cancel: CancellationToken,
}

impl ScanJob {
    pub fn state(&self) -> State {
        *self.state.lock().unwrap()
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The job as the API shows it.
    pub fn view(&self) -> Value {
        let found: Vec<Value> = self
            .found
            .lock()
            .unwrap()
            .iter()
            .map(|f| json!({ "daemonId": f.daemon_id, "name": f.name, "addrs": f.addrs, "port": f.port, "pairable": f.pairable }))
            .collect();
        json!({
            "scanId": self.id,
            "state": self.state(),
            "scanned": self.scanned.load(Ordering::Relaxed),
            "total": self.total,
            "port": self.port,
            "ranges": self.ranges,
            "notes": self.notes,
            "found": found,
        })
    }
}

#[derive(Debug)]
pub enum StartError {
    /// A scan is running; its id.
    Busy(String),
}

/// The scan slot of a daemon: one running scan, and the last finished one
/// until the next starts.
#[derive(Default)]
pub struct Scans {
    current: Mutex<Option<Arc<ScanJob>>>,
}

impl Scans {
    pub fn get(&self, id: &str) -> Option<Arc<ScanJob>> {
        self.current.lock().unwrap().clone().filter(|j| j.id == id)
    }

    /// Start walking `targets` on `port`. The job runs in the background.
    pub fn start(&self, st: &Arc<LinkState>, targets: Targets, port: u16) -> Result<Arc<ScanJob>, StartError> {
        let mut slot = self.current.lock().unwrap();
        if let Some(running) = slot.as_ref().filter(|j| j.state() == State::Running) {
            return Err(StartError::Busy(running.id.clone()));
        }
        let job = Arc::new(ScanJob {
            id: uuid::Uuid::new_v4().to_string(),
            port,
            ranges: targets.ranges,
            notes: targets.notes,
            total: targets.hosts.len(),
            scanned: AtomicUsize::new(0),
            state: Mutex::new(State::Running),
            found: Mutex::new(Vec::new()),
            cancel: CancellationToken::new(),
        });
        *slot = Some(Arc::clone(&job));
        tokio::spawn(run(Arc::clone(st), Arc::clone(&job), targets.hosts));
        Ok(job)
    }
}

/// Is a SLink listening at `ip:port`? Its identity when it is.
pub async fn probe_host(http: &reqwest::Client, ip: IpAddr, port: u16) -> Option<Found> {
    let ep = Endpoint { host: Host::Ip(ip), port };
    ep.check().ok()?;
    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(SocketAddr::new(ip, port))).await.ok()?.ok()?;
    let id = probe::identity_at(http, &ep, CARD_TIMEOUT).await?;
    Some(Found { daemon_id: id.daemon_id, name: id.name, addrs: vec![ip], port, pairable: id.pairable })
}

async fn run(st: Arc<LinkState>, job: Arc<ScanJob>, hosts: Vec<Ipv4Addr>) {
    let permits = Arc::new(Semaphore::new(CONCURRENCY));
    let mut tasks = JoinSet::new();
    for host in hosts {
        let permit = tokio::select! {
            p = Arc::clone(&permits).acquire_owned() => p.expect("the semaphore is never closed"),
            _ = job.cancel.cancelled() => break,
        };
        if job.cancel.is_cancelled() {
            break;
        }
        let (st, job) = (Arc::clone(&st), Arc::clone(&job));
        tasks.spawn(async move {
            let _permit = permit;
            if !job.cancel.is_cancelled() {
                if let Some(found) = probe_host(&st.http, IpAddr::V4(host), job.port).await {
                    if found.daemon_id != st.store.daemon_id() {
                        st.discovery.upsert(Source::Scan, found.clone());
                        job.found.lock().unwrap().push(found);
                    }
                }
            }
            job.scanned.fetch_add(1, Ordering::Relaxed);
        });
    }
    while tasks.join_next().await.is_some() {}
    *job.state.lock().unwrap() = if job.cancel.is_cancelled() { State::Canceled } else { State::Done };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sen_link::testkit;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn a_range_is_read_with_its_host_bits_ignored() {
        assert_eq!(parse_cidr("192.168.1.20/24").unwrap(), (ip("192.168.1.0"), 24));
        assert_eq!(parse_cidr(" 10.0.0.0/22 ").unwrap(), (ip("10.0.0.0"), 22));
        assert_eq!(parse_cidr("127.0.0.1/32").unwrap(), (ip("127.0.0.1"), 32));
        for bad in ["192.168.1.0", "192.168.1.0/33", "192.168.1.0/x", "fd00::/64", "host/24", "", "/24"] {
            assert!(parse_cidr(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_typed_range_must_be_lan_and_small() {
        let t = range_targets("192.168.1.0/24").unwrap();
        assert_eq!(t.hosts.len(), 254);
        assert!(!t.hosts.contains(&ip("192.168.1.0")) && !t.hosts.contains(&ip("192.168.1.255")) && t.hosts.contains(&ip("192.168.1.20")));
        assert_eq!(t.ranges, vec!["192.168.1.0/24".to_string()]);

        assert_eq!(range_targets("10.0.0.0/22").unwrap().hosts.len(), 1022);
        assert_eq!(range_targets("127.0.0.1/32").unwrap().hosts, vec![ip("127.0.0.1")]);
        assert_eq!(range_targets("192.168.1.20/32").unwrap().hosts, vec![ip("192.168.1.20")], "one address is a range too");
        assert_eq!(range_targets("127.0.0.0/30").unwrap().hosts, vec![ip("127.0.0.1"), ip("127.0.0.2")]);
        assert_eq!(range_targets("100.101.102.0/24").unwrap().hosts.len(), 254, "a tailnet slice is LAN");

        for (range, why) in [
            ("192.168.0.0/16", "too large"),
            ("10.0.0.0/21", "too large"),
            ("8.8.8.0/24", "not inside a private network"),
            ("172.16.0.0/11", "too large"),
            ("100.0.0.0/24", "not inside a private network"),
        ] {
            let e = range_targets(range).unwrap_err();
            assert!(e.contains(why), "{range}: {e}");
        }
    }

    #[test]
    fn a_machines_own_subnets_are_walked_and_the_odd_ones_are_skipped_or_narrowed() {
        let own = [ip("192.168.0.82"), ip("10.14.0.2"), ip("100.101.102.103"), ip("127.0.0.1")];
        let ifaces = [
            (ip("192.168.0.82"), 24),
            (ip("10.14.0.2"), 16),         // a VPN: wider than a /22 -> the /24 around it
            (ip("100.101.102.103"), 32),   // Tailscale: nothing to walk
            (ip("127.0.0.1"), 8),
            (ip("169.254.9.9"), 16),
            (ip("8.8.8.8"), 24),
            (ip("192.168.0.99"), 24),      // a second address in the same subnet
            (ip("172.20.0.5"), 31),
        ];
        let t = interface_targets(&ifaces, &own);
        assert_eq!(t.ranges, vec!["192.168.0.0/24".to_string(), "10.14.0.0/24".to_string()]);
        assert_eq!(t.hosts.len(), 253 + 253, "each /24 minus its network, broadcast and this machine");
        assert_eq!(t.notes.len(), 1);
        assert!(t.notes[0].contains("10.14.0.2/16") && t.notes[0].contains("10.14.0.0/24"), "{:?}", t.notes);

        // Past the cap the later subnets are left out, and the note says which.
        let many: Vec<(Ipv4Addr, u8)> = (0..6).map(|i| (Ipv4Addr::new(10, i, 0, 1), 24)).collect();
        let capped = interface_targets(&many, &[]);
        assert!(capped.hosts.len() <= MAX_HOSTS && capped.hosts.len() == 254 * 4, "{}", capped.hosts.len());
        assert_eq!(capped.notes.len(), 2);
        assert!(interface_targets(&[], &[]).hosts.is_empty());
    }

    #[test]
    fn the_numbers_of_a_subnet_are_right() {
        assert_eq!(hosts_in(u32::from(ip("192.168.1.0")), 24).len(), 254);
        assert_eq!(hosts_in(u32::from(ip("10.0.0.0")), 30), vec![ip("10.0.0.1"), ip("10.0.0.2")]);
        assert_eq!(hosts_in(u32::from(ip("10.0.0.4")), 31), vec![ip("10.0.0.4"), ip("10.0.0.5")]);
        assert_eq!(hosts_in(u32::from(ip("10.0.0.7")), 32), vec![ip("10.0.0.7")]);
    }

    fn job(total: usize, port: u16) -> Arc<ScanJob> {
        Arc::new(ScanJob {
            id: uuid::Uuid::new_v4().to_string(),
            port,
            ranges: Vec::new(),
            notes: Vec::new(),
            total,
            scanned: AtomicUsize::new(0),
            state: Mutex::new(State::Running),
            found: Mutex::new(Vec::new()),
            cancel: CancellationToken::new(),
        })
    }

    #[tokio::test]
    async fn only_a_real_slink_counts_as_found() {
        let a = testkit::node("Mac A", json!({})).await;
        let b = testkit::node("Mac B", json!({ "discoverable": false })).await;
        let c = testkit::node("Mac C", json!({})).await;
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let found = probe_host(&a.st.http, lo, c.port).await.unwrap();
        assert_eq!((found.name.as_str(), found.daemon_id.as_str(), found.port, found.pairable), ("Mac C", c.st.store.daemon_id(), c.port, false));
        assert!(probe_host(&a.st.http, lo, b.port).await.is_none(), "a SLink that hides its card is not found");
        assert!(probe_host(&a.st.http, lo, testkit::free_port()).await.is_none(), "a closed port");
        assert!(probe_host(&a.st.http, IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 18791).await.is_none(), "never dials outside the LAN");

        // Some other web server whose card path answers 200 with something else.
        let app = axum::Router::new().route("/.well-known/agent-card.json", axum::routing::get(|| async { "<html>hi</html>" }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let web = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        assert!(probe_host(&a.st.http, lo, port).await.is_none());
        web.abort();
    }

    #[tokio::test]
    async fn one_scan_at_a_time_and_a_cancelled_one_ends_canceled() {
        let a = testkit::node("Mac A", json!({})).await;
        let running = job(10, 1);
        *a.st.scans.current.lock().unwrap() = Some(Arc::clone(&running));
        let t = Targets { hosts: vec![Ipv4Addr::LOCALHOST], ranges: vec!["127.0.0.1/32".into()], notes: vec![] };
        match a.st.scans.start(&a.st, t, 1) {
            Err(StartError::Busy(id)) => assert_eq!(id, running.id),
            Ok(_) => panic!("a second scan started"),
        }
        assert!(a.st.scans.get(&running.id).is_some() && a.st.scans.get("other").is_none());

        // Cancelled before it begins: nothing is visited and the state says so.
        let j = job(5, testkit::free_port());
        j.cancel();
        run(Arc::clone(&a.st), Arc::clone(&j), vec![Ipv4Addr::LOCALHOST; 5]).await;
        assert_eq!((j.state(), j.scanned.load(Ordering::Relaxed)), (State::Canceled, 0));
        assert_eq!(j.view()["state"], "canceled");

        // Finished, it makes room for the next.
        *running.state.lock().unwrap() = State::Done;
        let started = a.st.scans.start(&a.st, Targets { hosts: vec![Ipv4Addr::LOCALHOST], ranges: vec![], notes: vec![] }, testkit::free_port()).unwrap();
        for _ in 0..100 {
            if started.state() != State::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!((started.state(), started.scanned.load(Ordering::Relaxed)), (State::Done, 1));
    }

    #[test]
    fn this_machines_interfaces_read_without_surprises() {
        let (ifaces, own) = local_ipv4();
        assert!(ifaces.iter().all(|(ip, prefix)| !ip.is_loopback() && *prefix <= 32 && own.contains(ip)));
    }
}
