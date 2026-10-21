//! Finding other SLinks on the LAN without typing an address.
//!
//! Two sources feed one table, keyed by the other daemon's id:
//!
//! * **mDNS / DNS-SD** (`_senclaw-link._tcp`): while SLink is enabled this
//!   daemon browses, and while it is also discoverable and listening on a real
//!   interface it announces itself (`id`, `name`, `pair` in the TXT record, the
//!   listener's port). Passive and automatic.
//! * **A scan** (`scan.rs`), which a person starts.
//!
//! Neither is authenticated: a name on the list is a claim. What proves who is
//! on the other end is the pairing code, not this table.
//!
//! An entry expires [`SEEN_TTL`] after its last sighting, but a responder that
//! has not changed is not announced again, so a daemon that is still there would
//! drop off. Before that happens the daemon asks it (its Agent Card, at the
//! addresses it was seen at) and a card with the same id counts as a sighting.

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::future::join_all;
use mdns_sd::{IfKind, IfPredicate, ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::Serialize;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::lan::{self, Endpoint, Host};
use super::probe;
use super::store::clean_daemon_id;
use super::LinkState;

pub const SERVICE_TYPE: &str = "_senclaw-link._tcp.local.";
/// How long an entry survives without being seen again (a scan hit, or an mDNS
/// answer whose goodbye was lost).
const SEEN_TTL: Duration = Duration::from_secs(300);
/// How often the supervisor looks again when nobody woke it (an invite
/// expiring changes what is announced).
const RECHECK: Duration = Duration::from_secs(5);
/// An entry not seen for this long is asked again instead of waiting for its
/// time to run out; half of [`SEEN_TTL`] leaves room for several tries.
const REVALIDATE_AFTER: Duration = Duration::from_secs(150);
/// How often entries due for that are looked at.
const REVALIDATE_EVERY: Duration = Duration::from_secs(15);
/// Entries asked per pass (the longest-unseen first) and addresses tried per entry.
const REVALIDATE_ENTRIES: usize = 4;
const REVALIDATE_ADDRS: usize = 4;
/// One Agent Card request.
const REVALIDATE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Mdns,
    Scan,
}

/// What one sighting says about a daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub daemon_id: String,
    pub name: String,
    pub addrs: Vec<IpAddr>,
    pub port: u16,
    /// It has a live invite, so a code can be redeemed there now.
    pub pairable: bool,
}

/// A daemon as the API lists it: every source folded together.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Discovered {
    pub daemon_id: String,
    pub name: String,
    /// LAN addresses, IPv4 first.
    pub addrs: Vec<IpAddr>,
    pub port: u16,
    pub pairable: bool,
    pub sources: Vec<Source>,
    pub seen_secs_ago: u64,
}

impl Discovered {
    /// Where to try it, best first.
    pub fn endpoints(&self) -> Vec<Endpoint> {
        self.addrs.iter().map(|ip| Endpoint { host: Host::Ip(*ip), port: self.port }).collect()
    }
}

struct Seen {
    at: Instant,
    found: Found,
}

#[derive(Default)]
struct Table {
    entries: HashMap<String, HashMap<Source, Seen>>,
    /// mDNS instance name → daemon id, so a goodbye can be matched to its entry.
    mdns_names: HashMap<String, String>,
}

pub struct Discovery {
    own_id: String,
    table: Mutex<Table>,
    browsing: AtomicBool,
    advertising: AtomicBool,
    error: Mutex<Option<String>>,
}

/// Addresses worth trying: on the LAN, not loopback, and not an IPv6
/// link-local one (it needs a scope id that a URL cannot carry). IPv4 first.
pub fn usable_addrs(addrs: impl IntoIterator<Item = IpAddr>) -> Vec<IpAddr> {
    let mut v: Vec<IpAddr> = addrs
        .into_iter()
        .filter(|ip| lan::is_lan_addr(*ip) && !ip.is_loopback() && !matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80))
        .collect();
    v.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    v.dedup();
    v
}

/// What an mDNS announcement says, or `None` when it is not a usable SLink.
/// `txt` reads a TXT property; `port` and `addrs` come from the SRV and address records.
pub fn found_from_announcement(txt: impl Fn(&str) -> Option<String>, port: u16, addrs: impl IntoIterator<Item = IpAddr>) -> Option<Found> {
    let daemon_id = clean_daemon_id(&txt("id")?)?;
    let name: String = txt("name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "SenClaw".into()).trim().chars().take(80).collect();
    let addrs = usable_addrs(addrs);
    if port == 0 || addrs.is_empty() {
        return None;
    }
    Some(Found { daemon_id, name, addrs, port, pairable: txt("pair").as_deref() == Some("1") })
}

impl Discovery {
    pub fn new(own_id: String) -> Self {
        Self { own_id, table: Mutex::new(Table::default()), browsing: AtomicBool::new(false), advertising: AtomicBool::new(false), error: Mutex::new(None) }
    }

    pub fn upsert(&self, source: Source, found: Found) {
        self.upsert_at(source, found, Instant::now());
    }

    fn upsert_at(&self, source: Source, found: Found, at: Instant) {
        if found.daemon_id == self.own_id {
            return;
        }
        let mut t = self.table.lock().unwrap();
        t.entries.entry(found.daemon_id.clone()).or_default().insert(source, Seen { at, found });
    }

    /// An mDNS sighting, remembered under its instance name for the goodbye.
    /// DNS names are not case-sensitive and the responder reports them lowercased
    /// in a goodbye, so the key is too.
    pub fn upsert_mdns(&self, instance: &str, found: Found) {
        if found.daemon_id == self.own_id {
            return;
        }
        self.table.lock().unwrap().mdns_names.insert(instance.to_lowercase(), found.daemon_id.clone());
        self.upsert(Source::Mdns, found);
    }

    /// An mDNS goodbye (or expiry): the daemon is no longer *announced*; a scan
    /// that saw it stays until it ages out.
    pub fn remove_mdns(&self, instance: &str) {
        let mut t = self.table.lock().unwrap();
        if let Some(id) = t.mdns_names.remove(&instance.to_lowercase()) {
            if let Some(sources) = t.entries.get_mut(&id) {
                sources.remove(&Source::Mdns);
            }
        }
    }

    pub fn clear_source(&self, source: Source) {
        let mut t = self.table.lock().unwrap();
        for sources in t.entries.values_mut() {
            sources.remove(&source);
        }
        if source == Source::Mdns {
            t.mdns_names.clear();
        }
    }

    pub fn list(&self) -> Vec<Discovered> {
        self.list_at(Instant::now())
    }

    fn list_at(&self, now: Instant) -> Vec<Discovered> {
        let mut t = self.table.lock().unwrap();
        for sources in t.entries.values_mut() {
            sources.retain(|_, s| now.saturating_duration_since(s.at) <= SEEN_TTL);
        }
        t.entries.retain(|_, sources| !sources.is_empty());
        let live: std::collections::HashSet<String> = t.entries.keys().cloned().collect();
        t.mdns_names.retain(|_, id| live.contains(id));

        let mut out: Vec<Discovered> = t
            .entries
            .iter()
            .map(|(id, sources)| {
                // The freshest sighting names it; every source's addresses count.
                let latest = sources.values().max_by_key(|s| s.at).expect("entries are never empty");
                let mut srcs: Vec<Source> = sources.keys().copied().collect();
                srcs.sort();
                Discovered {
                    daemon_id: id.clone(),
                    name: latest.found.name.clone(),
                    addrs: usable_or_all(sources.values().flat_map(|s| s.found.addrs.iter().copied())),
                    port: latest.found.port,
                    pairable: latest.found.pairable,
                    sources: srcs,
                    seen_secs_ago: now.saturating_duration_since(latest.at).as_secs(),
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.daemon_id.cmp(&b.daemon_id)));
        out
    }

    /// Entries whose freshest sighting is old enough to ask about but has not
    /// yet expired, longest-unseen first, with the endpoints to ask at.
    fn due_for_revalidation(&self, now: Instant) -> Vec<(String, Vec<Endpoint>)> {
        let t = self.table.lock().unwrap();
        let mut due: Vec<(Duration, String, Vec<Endpoint>)> = t
            .entries
            .iter()
            .filter_map(|(id, sources)| {
                let age = now.saturating_duration_since(sources.values().map(|s| s.at).max()?);
                if age <= REVALIDATE_AFTER || age > SEEN_TTL {
                    return None;
                }
                let mut at: Vec<(IpAddr, u16)> = sources.values().flat_map(|s| s.found.addrs.iter().map(|ip| (*ip, s.found.port))).collect();
                at.sort_by_key(|(ip, port)| (ip.is_loopback(), ip.is_ipv6(), *ip, *port));
                at.dedup();
                let eps = at.into_iter().take(REVALIDATE_ADDRS).map(|(ip, port)| Endpoint { host: Host::Ip(ip), port }).collect();
                Some((age, id.clone(), eps))
            })
            .collect();
        due.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        due.into_iter().take(REVALIDATE_ENTRIES).map(|(_, id, eps)| (id, eps)).collect()
    }

    /// Count a sighting for an entry that is still listed, whatever its source:
    /// each source keeps its own label and only its clock moves. A goodbye that
    /// landed while the daemon was being asked is not undone.
    fn refresh(&self, daemon_id: &str, now: Instant) {
        if let Some(sources) = self.table.lock().unwrap().entries.get_mut(daemon_id) {
            for s in sources.values_mut() {
                s.at = s.at.max(now);
            }
        }
    }

    /// Ask the entries that have gone quiet whether they are still there.
    /// `ask` returns the daemon id that answers at an endpoint, if any; only the
    /// *same* id keeps an entry alive, so an address another daemon has taken
    /// does not. Entries that do not answer are left to age out. Returns how
    /// many were confirmed.
    async fn revalidate_at<A, Fut>(&self, now: Instant, ask: A) -> usize
    where
        A: Fn(Endpoint) -> Fut,
        Fut: Future<Output = Option<String>>,
    {
        let ask = &ask;
        let answers = join_all(self.due_for_revalidation(now).into_iter().map(|(id, eps)| async move {
            for ep in eps {
                if ask(ep).await.as_deref() == Some(id.as_str()) {
                    return Some(id);
                }
            }
            None
        }))
        .await;
        let confirmed: Vec<String> = answers.into_iter().flatten().collect();
        for id in &confirmed {
            self.refresh(id, now);
        }
        confirmed.len()
    }

    pub fn find(&self, daemon_id: &str) -> Option<Discovered> {
        self.list().into_iter().find(|d| d.daemon_id == daemon_id)
    }

    pub fn contains(&self, daemon_id: &str) -> bool {
        self.find(daemon_id).is_some()
    }

    pub fn browsing(&self) -> bool {
        self.browsing.load(Ordering::Relaxed)
    }

    pub fn advertising(&self) -> bool {
        self.advertising.load(Ordering::Relaxed)
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    fn set_activity(&self, browsing: bool, advertising: bool, error: Option<String>) {
        self.browsing.store(browsing, Ordering::Relaxed);
        self.advertising.store(advertising, Ordering::Relaxed);
        *self.error.lock().unwrap() = error;
    }
}

/// Sort and dedupe addresses the way [`usable_addrs`] does but keep a loopback
/// one a scan found on this machine (a test, or a second daemon here).
fn usable_or_all(addrs: impl IntoIterator<Item = IpAddr>) -> Vec<IpAddr> {
    let mut v: Vec<IpAddr> = addrs.into_iter().collect();
    v.sort_by_key(|ip| (ip.is_loopback(), ip.is_ipv6(), *ip));
    v.dedup();
    v
}

// ---- the mDNS side --------------------------------------------------------

/// What this daemon announces.
#[derive(Debug, Clone, PartialEq)]
struct Advert {
    name: String,
    id: String,
    port: u16,
    pairable: bool,
}

impl Advert {
    fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(8)]
    }

    /// The instance name: the display name (dots and control characters made
    /// safe for a DNS label) and a piece of the id, so two Macs both called
    /// "SenClaw" are two instances.
    fn instance(&self) -> String {
        let name: String = self.name.chars().filter(|c| !c.is_control()).map(|c| if c == '.' { '-' } else { c }).take(40).collect();
        format!("{}-{}", name.trim(), self.short_id())
    }

    fn info(&self) -> anyhow::Result<ServiceInfo> {
        let props: HashMap<String, String> = [
            ("v", "1".to_string()),
            ("id", self.id.clone()),
            ("name", self.name.chars().take(80).collect()),
            ("pair", if self.pairable { "1" } else { "0" }.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let host = format!("senclaw-{}.local.", self.short_id());
        Ok(ServiceInfo::new(SERVICE_TYPE, &self.instance(), &host, "", self.port, props).context("describe the SLink service")?.enable_addr_auto())
    }
}

struct Want {
    browse: bool,
    advert: Option<Advert>,
}

fn wanted(st: &LinkState) -> Want {
    let s = st.settings();
    // Announce only what a peer can actually reach: a listener on a real interface.
    let advert = (s.enabled && s.discoverable)
        .then(|| st.listen_status().addr)
        .flatten()
        .filter(|a| !a.ip().is_loopback())
        .map(|a| Advert { name: s.name.clone(), id: st.store.daemon_id().to_string(), port: a.port(), pairable: st.store.has_live_invite(crate::kanban::api::now()) });
    Want { browse: s.enabled, advert }
}

/// Only interfaces a LAN peer could be on: LAN-class address, not loopback,
/// not an IPv6 link-local one.
fn lan_interfaces() -> IfPredicate {
    IfPredicate::new(|i| {
        let ip = i.ip();
        !i.is_loopback() && lan::is_lan_addr(ip) && !matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80)
    })
}

struct Mdns {
    daemon: ServiceDaemon,
    browse: Option<JoinHandle<()>>,
    advert: Option<(Advert, String)>,
}

impl Mdns {
    fn start() -> anyhow::Result<Self> {
        let daemon = ServiceDaemon::new().context("start the mDNS responder")?;
        daemon.disable_interface(IfKind::All).context("restrict mDNS interfaces")?;
        daemon.enable_interface(IfKind::Predicate(lan_interfaces())).context("restrict mDNS interfaces")?;
        Ok(Self { daemon, browse: None, advert: None })
    }

    fn stop_browse(&mut self) {
        if let Some(task) = self.browse.take() {
            task.abort();
            let _ = self.daemon.stop_browse(SERVICE_TYPE);
        }
    }
}

impl Drop for Mdns {
    fn drop(&mut self) {
        self.stop_browse();
        if let Some((_, fullname)) = self.advert.take() {
            let _ = self.daemon.unregister(&fullname);
        }
        let _ = self.daemon.shutdown();
    }
}

fn spawn_browse(daemon: &ServiceDaemon, discovery: Arc<Discovery>) -> anyhow::Result<JoinHandle<()>> {
    let rx = daemon.browse(SERVICE_TYPE).context("browse for SLinks")?;
    Ok(tokio::spawn(async move {
        while let Ok(event) = rx.recv_async().await {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let found = found_from_announcement(
                        |k| info.get_property_val_str(k).map(str::to_string),
                        info.get_port(),
                        info.get_addresses().iter().map(|a| a.to_ip_addr()),
                    );
                    if let Some(found) = found {
                        discovery.upsert_mdns(info.get_fullname(), found);
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => discovery.remove_mdns(&fullname),
                _ => {}
            }
        }
    }))
}

fn reconcile(mdns: &mut Option<Mdns>, want: &Want, st: &LinkState) -> anyhow::Result<()> {
    if !want.browse && want.advert.is_none() {
        *mdns = None;
        st.discovery.clear_source(Source::Mdns);
        return Ok(());
    }
    if mdns.is_none() {
        *mdns = Some(Mdns::start()?);
    }
    let m = mdns.as_mut().expect("started above");

    match (want.browse, m.browse.is_some()) {
        (true, false) => m.browse = Some(spawn_browse(&m.daemon, Arc::clone(&st.discovery))?),
        (false, true) => {
            m.stop_browse();
            st.discovery.clear_source(Source::Mdns);
        }
        _ => {}
    }

    if m.advert.as_ref().map(|(a, _)| a) != want.advert.as_ref() {
        match &want.advert {
            // Registering again with the same name updates the announcement.
            Some(a) => {
                let info = a.info()?;
                let fullname = info.get_fullname().to_string();
                m.daemon.register(info).context("announce this SLink")?;
                m.advert = Some((a.clone(), fullname));
            }
            None => {
                if let Some((_, fullname)) = m.advert.take() {
                    let _ = m.daemon.unregister(&fullname);
                }
            }
        }
    }
    Ok(())
}

/// Start the supervisor: it browses while SLink is on and announces while this
/// daemon is also discoverable and listening. Abort the handle to stop it.
pub fn start(st: &Arc<LinkState>) -> JoinHandle<()> {
    let (supervisor, revalidator) = (supervise(Arc::downgrade(st), Arc::clone(&st.discovery_wake)), revalidate_loop(Arc::downgrade(st)));
    tokio::spawn(async move {
        tokio::join!(supervisor, revalidator);
    })
}

/// Keep daemons that are still on the network from expiring: every
/// [`REVALIDATE_EVERY`], ask the ones that have gone quiet. Its own loop, so a
/// slow answer never delays the mDNS supervisor.
async fn revalidate_loop(weak: Weak<LinkState>) {
    loop {
        tokio::time::sleep(REVALIDATE_EVERY).await;
        let Some(st) = weak.upgrade() else { break };
        // Nothing is sent while SLink is off.
        if st.settings().enabled {
            let http = &st.http;
            let ask = |ep: Endpoint| async move { probe::identity_at(http, &ep, REVALIDATE_TIMEOUT).await.map(|i| i.daemon_id) };
            st.discovery.revalidate_at(Instant::now(), ask).await;
        }
    }
}

async fn supervise(weak: Weak<LinkState>, wake: Arc<Notify>) {
    let mut mdns: Option<Mdns> = None;
    let mut last_error: Option<String> = None;
    loop {
        let Some(st) = weak.upgrade() else { break };
        let want = wanted(&st);
        let error = match reconcile(&mut mdns, &want, &st) {
            Ok(()) => None,
            Err(e) => {
                // Start from scratch next time rather than keep a half-set-up responder.
                mdns = None;
                Some(format!("{e:#}"))
            }
        };
        if error != last_error {
            if let Some(e) = &error {
                tracing::warn!("[slink] mDNS: {e}");
            }
            last_error = error.clone();
        }
        st.discovery.set_activity(mdns.as_ref().is_some_and(|m| m.browse.is_some()), mdns.as_ref().is_some_and(|m| m.advert.is_some()), error);
        drop(st);
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(RECHECK) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn found(id: &str, name: &str, addrs: &[&str], port: u16, pairable: bool) -> Found {
        Found { daemon_id: id.into(), name: name.into(), addrs: addrs.iter().map(|a| ip(a)).collect(), port, pairable }
    }

    #[test]
    fn only_addresses_a_peer_could_use_are_kept_and_ipv4_comes_first() {
        let kept = usable_addrs([
            ip("fd00::5"), ip("192.168.1.20"), ip("8.8.8.8"), ip("127.0.0.1"), ip("fe80::1"), ip("100.101.102.103"), ip("::1"), ip("192.168.1.20"), ip("10.0.0.2"),
        ]);
        assert_eq!(kept, vec![ip("10.0.0.2"), ip("100.101.102.103"), ip("192.168.1.20"), ip("fd00::5")]);
    }

    #[test]
    fn an_announcement_is_read_for_what_a_slink_says_and_nothing_else() {
        let txt = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string());
        let ok = found_from_announcement(txt(&[("id", "6f1c-0a"), ("name", "  Mac mini "), ("pair", "1")]), 18791, [ip("192.168.1.20"), ip("8.8.8.8")]).unwrap();
        assert_eq!((ok.daemon_id.as_str(), ok.name.as_str(), ok.port, ok.pairable), ("6f1c-0a", "Mac mini", 18791, true));
        assert_eq!(ok.addrs, vec![ip("192.168.1.20")], "a public address in the record is dropped");
        assert!(!found_from_announcement(txt(&[("id", "6f1c-0a"), ("pair", "0")]), 18791, [ip("10.0.0.2")]).unwrap().pairable);
        assert_eq!(found_from_announcement(txt(&[("id", "6f1c-0a")]), 18791, [ip("10.0.0.2")]).unwrap().name, "SenClaw");

        for (what, f) in [
            ("no id", found_from_announcement(txt(&[("name", "x")]), 18791, [ip("10.0.0.2")])),
            ("an id that is not printable", found_from_announcement(txt(&[("id", "<b>")]), 18791, [ip("10.0.0.2")])),
            ("port 0", found_from_announcement(txt(&[("id", "a")]), 0, [ip("10.0.0.2")])),
            ("no usable address", found_from_announcement(txt(&[("id", "a")]), 1, [ip("8.8.8.8"), ip("127.0.0.1")])),
        ] {
            assert!(f.is_none(), "{what}");
        }
    }

    #[test]
    fn sightings_from_two_sources_are_one_entry_and_this_daemon_is_never_listed() {
        let d = Discovery::new("me".into());
        let t0 = Instant::now();
        d.upsert_at(Source::Mdns, found("b", "Mac B", &["192.168.1.20", "fd00::20"], 18791, false), t0);
        d.upsert_at(Source::Scan, found("b", "Mac B (scan)", &["192.168.1.20", "10.14.0.9"], 18791, true), t0 + Duration::from_secs(10));
        d.upsert_at(Source::Mdns, found("me", "Me", &["192.168.1.5"], 18791, false), t0);
        d.upsert_mdns("me._senclaw-link._tcp.local.", found("me", "Me", &["192.168.1.5"], 18791, false));

        let list = d.list_at(t0 + Duration::from_secs(20));
        assert_eq!(list.len(), 1);
        let b = &list[0];
        assert_eq!(b.name, "Mac B (scan)", "the freshest sighting names it");
        assert!(b.pairable);
        assert_eq!(b.sources, vec![Source::Mdns, Source::Scan]);
        assert_eq!(b.addrs, vec![ip("10.14.0.9"), ip("192.168.1.20"), ip("fd00::20")], "every source's addresses, IPv4 first");
        assert_eq!(b.seen_secs_ago, 10);
        assert_eq!(b.endpoints()[1].to_string(), "192.168.1.20:18791");
    }

    #[test]
    fn entries_age_out_and_a_goodbye_removes_only_the_announcement() {
        let d = Discovery::new("me".into());
        let t0 = Instant::now();
        d.upsert_at(Source::Scan, found("b", "Mac B", &["192.168.1.20"], 18791, false), t0);
        d.upsert_at(Source::Mdns, found("c", "Mac C", &["192.168.1.30"], 18791, false), t0 + Duration::from_secs(200));
        d.table.lock().unwrap().mdns_names.insert("c-inst".into(), "c".into());

        let later = t0 + SEEN_TTL + Duration::from_secs(1);
        let ids: Vec<String> = d.list_at(later).into_iter().map(|x| x.daemon_id).collect();
        assert_eq!(ids, vec!["c"], "the old scan hit is gone, the newer mDNS one stays");

        d.upsert_at(Source::Scan, found("c", "Mac C", &["192.168.1.30"], 18791, false), later);
        d.remove_mdns("c-inst");
        let c = &d.list_at(later)[0];
        assert_eq!(c.sources, vec![Source::Scan], "a goodbye takes away the announcement, not the scan hit");
        d.remove_mdns("c-inst");
        d.clear_source(Source::Scan);
        assert!(d.list_at(later).is_empty());
    }

    /// `ask` for [`Discovery::revalidate_at`]: `answers` maps an address to the id
    /// that answers there; every endpoint asked is recorded.
    fn asking(answers: &'static [(&'static str, &'static str)], asked: &Arc<Mutex<Vec<String>>>) -> impl Fn(Endpoint) -> std::future::Ready<Option<String>> {
        let asked = Arc::clone(asked);
        move |ep: Endpoint| {
            asked.lock().unwrap().push(ep.to_string());
            std::future::ready(answers.iter().find(|(addr, _)| *addr == ep.to_string()).map(|(_, id)| id.to_string()))
        }
    }

    fn ids(d: &Discovery, at: Instant) -> Vec<String> {
        d.list_at(at).into_iter().map(|x| x.daemon_id).collect()
    }

    #[tokio::test]
    async fn a_quiet_entry_that_still_answers_outlives_the_ttl_and_one_that_does_not_is_dropped() {
        let d = Discovery::new("me".into());
        let t0 = Instant::now();
        d.upsert_at(Source::Mdns, found("up", "Mac Up", &["192.168.1.20"], 18791, false), t0);
        d.upsert_at(Source::Mdns, found("down", "Mac Down", &["192.168.1.30"], 18791, false), t0);
        let asked = Arc::new(Mutex::new(Vec::new()));

        let now = t0 + REVALIDATE_AFTER + Duration::from_secs(1);
        let n = d.revalidate_at(now, asking(&[("192.168.1.20:18791", "up")], &asked)).await;
        assert_eq!(n, 1);

        let after_ttl = t0 + SEEN_TTL + Duration::from_secs(1);
        assert_eq!(ids(&d, after_ttl), vec!["up"], "the answering daemon stays, the silent one ages out");
        let up = &d.list_at(after_ttl)[0];
        assert_eq!(up.sources, vec![Source::Mdns], "it is still an announcement, not a scan hit");
        assert_eq!(up.seen_secs_ago, (after_ttl - now).as_secs());

        // Asked again once the new sighting is itself old, and again after that.
        let later = now + REVALIDATE_AFTER + Duration::from_secs(1);
        assert_eq!(d.revalidate_at(later, asking(&[("192.168.1.20:18791", "up")], &asked)).await, 1);
        assert_eq!(ids(&d, t0 + SEEN_TTL * 2), vec!["up"]);
    }

    #[tokio::test]
    async fn another_daemon_at_the_address_does_not_keep_an_entry_alive() {
        let d = Discovery::new("me".into());
        let t0 = Instant::now();
        d.upsert_at(Source::Mdns, found("b", "Mac B", &["192.168.1.20", "192.168.1.21"], 18791, false), t0);
        let asked = Arc::new(Mutex::new(Vec::new()));
        let now = t0 + REVALIDATE_AFTER + Duration::from_secs(1);

        assert_eq!(d.revalidate_at(now, asking(&[("192.168.1.20:18791", "someone-else")], &asked)).await, 0);
        assert_eq!(asked.lock().unwrap().len(), 2, "every known address is tried");
        assert!(ids(&d, t0 + SEEN_TTL + Duration::from_secs(1)).is_empty());

        // The second address is the right daemon: the first answer does not stop the search.
        d.upsert_at(Source::Mdns, found("b", "Mac B", &["192.168.1.20", "192.168.1.21"], 18791, false), t0);
        let n = d.revalidate_at(now, asking(&[("192.168.1.20:18791", "someone-else"), ("192.168.1.21:18791", "b")], &asked)).await;
        assert_eq!(n, 1);
        assert_eq!(ids(&d, t0 + SEEN_TTL + Duration::from_secs(1)), vec!["b"]);
    }

    #[tokio::test]
    async fn only_quiet_unexpired_entries_are_asked_a_few_at_a_time() {
        let d = Discovery::new("me".into());
        let t0 = Instant::now();
        d.upsert_at(Source::Mdns, found("fresh", "Fresh", &["192.168.1.10"], 18791, false), t0 + Duration::from_secs(100));
        d.upsert_at(Source::Scan, found("expired", "Expired", &["192.168.1.11"], 18791, false), t0 - Duration::from_secs(200));
        for i in 0..6u8 {
            // Entry 0 has been quiet longest.
            d.upsert_at(Source::Mdns, found(&format!("q{i}"), "Quiet", &[&format!("192.168.1.{}", 20 + i)], 18791, false), t0 + Duration::from_secs(u64::from(i)));
        }
        let asked = Arc::new(Mutex::new(Vec::new()));
        d.revalidate_at(t0 + REVALIDATE_AFTER + Duration::from_secs(10), asking(&[], &asked)).await;

        let mut asked = asked.lock().unwrap().clone();
        asked.sort();
        assert_eq!(asked, vec!["192.168.1.20:18791", "192.168.1.21:18791", "192.168.1.22:18791", "192.168.1.23:18791"], "the four longest-quiet, not the fresh or the expired");
    }

    #[tokio::test]
    async fn a_goodbye_that_arrives_while_asking_is_not_undone_and_a_scan_hit_is_kept_alive_as_a_scan_hit() {
        let d = Arc::new(Discovery::new("me".into()));
        let t0 = Instant::now();
        d.upsert_mdns("b-inst._senclaw-link._tcp.local.", found("b", "Mac B", &["192.168.1.20"], 18791, false));
        d.upsert_at(Source::Mdns, found("b", "Mac B", &["192.168.1.20"], 18791, false), t0);
        d.upsert_at(Source::Scan, found("c", "Mac C", &["192.168.1.30"], 18791, false), t0);
        let now = t0 + REVALIDATE_AFTER + Duration::from_secs(1);

        let dd = Arc::clone(&d);
        let ask = move |ep: Endpoint| {
            // The goodbye lands between the question and the answer.
            dd.remove_mdns("B-INST._senclaw-link._tcp.local.");
            let id = if ep.to_string().starts_with("192.168.1.20") { "b" } else { "c" };
            std::future::ready(Some(id.to_string()))
        };
        d.revalidate_at(now, ask).await;

        let after_ttl = t0 + SEEN_TTL + Duration::from_secs(1);
        let list = d.list_at(after_ttl);
        assert_eq!(list.iter().map(|x| (x.daemon_id.as_str(), x.sources.clone())).collect::<Vec<_>>(), vec![("c", vec![Source::Scan])]);
    }

    #[test]
    fn the_revalidation_window_fits_inside_the_ttl_with_room_to_retry() {
        assert!(REVALIDATE_AFTER * 2 <= SEEN_TTL);
        assert!(SEEN_TTL - REVALIDATE_AFTER >= REVALIDATE_EVERY * 4);
    }

    #[test]
    fn a_goodbye_finds_its_entry_whatever_the_case_of_the_name() {
        let d = Discovery::new("me".into());
        d.upsert_mdns("Mac B-290256f7._senclaw-link._tcp.local.", found("b", "Mac B", &["192.168.1.20"], 18791, false));
        assert_eq!(d.list().len(), 1);
        d.remove_mdns("mac b-290256f7._senclaw-link._tcp.local.");
        assert!(d.list().is_empty(), "the responder reports the name lowercased");
    }

    #[test]
    fn the_announcement_names_an_instance_that_is_a_safe_label_and_unique_per_daemon() {
        let a = Advert { name: "Benji's Mac.local".into(), id: "6f1c0a7e-1111-2222-3333-444455556666".into(), port: 18791, pairable: true };
        assert_eq!(a.instance(), "Benji's Mac-local-6f1c0a7e");
        let b = Advert { id: "abcdef12-0000".into(), ..a.clone() };
        assert_ne!(a.instance(), b.instance());
        let info = a.info().unwrap();
        assert_eq!(info.get_fullname(), "Benji's Mac-local-6f1c0a7e._senclaw-link._tcp.local.");
        assert_eq!(info.get_port(), 18791);
        assert_eq!(info.get_property_val_str("id"), Some(a.id.as_str()));
        assert_eq!(info.get_property_val_str("pair"), Some("1"));
        assert!(info.is_addr_auto());
    }
}
