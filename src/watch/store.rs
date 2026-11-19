//! Watch event store: append-only `events.log` (one TSV line per event, read
//! markers appended as `R`/`A` lines), pruned to 30 days / 50k events on open.
//! RAM holds only the newest 500.

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

pub const RAM_EVENTS: usize = 500;
pub const MAX_EVENTS: usize = 50_000;
pub const MAX_AGE_SECS: u64 = 30 * 24 * 3600;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    ServiceInstalled,
    DriverInstalled,
    ServiceRemoved,
    DriverRemoved,
    ServiceStarted,
    ServiceStopped,
    StartTypeChanged,
    ServiceCrashed,
    NewApp,
    AppUpdated,
    Camera,
    Microphone,
    Location,
    MemoryLeak,
    MemoryPressure,
    /// a network incident (the verdict stayed Degraded or Offline), see `netincident`
    Network,
}

impl Kind {
    pub const ALL: [Kind; 16] = [
        Kind::ServiceInstalled,
        Kind::DriverInstalled,
        Kind::ServiceRemoved,
        Kind::DriverRemoved,
        Kind::ServiceStarted,
        Kind::ServiceStopped,
        Kind::StartTypeChanged,
        Kind::ServiceCrashed,
        Kind::NewApp,
        Kind::AppUpdated,
        Kind::Camera,
        Kind::Microphone,
        Kind::Location,
        Kind::MemoryLeak,
        Kind::MemoryPressure,
        Kind::Network,
    ];
    pub fn key(self) -> &'static str {
        match self {
            Kind::ServiceInstalled => "svc_installed",
            Kind::DriverInstalled => "drv_installed",
            Kind::ServiceRemoved => "svc_removed",
            Kind::DriverRemoved => "drv_removed",
            Kind::ServiceStarted => "svc_started",
            Kind::ServiceStopped => "svc_stopped",
            Kind::StartTypeChanged => "start_type",
            Kind::ServiceCrashed => "svc_crashed",
            Kind::NewApp => "new_app",
            Kind::AppUpdated => "app_updated",
            Kind::Camera => "camera",
            Kind::Microphone => "microphone",
            Kind::Location => "location",
            Kind::MemoryLeak => "mem_leak",
            Kind::MemoryPressure => "mem_pressure",
            Kind::Network => "network",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Kind::ServiceInstalled => "Service installed",
            Kind::DriverInstalled => "Driver installed",
            Kind::ServiceRemoved => "Service removed",
            Kind::DriverRemoved => "Driver removed",
            Kind::ServiceStarted => "Service started",
            Kind::ServiceStopped => "Service stopped",
            Kind::StartTypeChanged => "Start type changed",
            Kind::ServiceCrashed => "Service crashed",
            Kind::NewApp => "New app launched",
            Kind::AppUpdated => "App updated",
            Kind::Camera => "Camera used",
            Kind::Microphone => "Microphone used",
            Kind::Location => "Location used",
            Kind::MemoryLeak => "Memory leak suspected",
            Kind::MemoryPressure => "Memory almost full",
            Kind::Network => "Network incident",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.key() == s)
    }
    pub fn is_service(self) -> bool {
        matches!(
            self,
            Kind::ServiceInstalled | Kind::DriverInstalled | Kind::ServiceRemoved | Kind::DriverRemoved | Kind::ServiceStarted | Kind::ServiceStopped | Kind::StartTypeChanged | Kind::ServiceCrashed
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SigState {
    #[default]
    Unknown,
    /// embedded Authenticode, chain OK
    Signed,
    /// signed via a Windows catalog (most OS files)
    Catalog,
    /// signed but chain/digest bad, expired, untrusted root
    Invalid,
    Unsigned,
}

impl SigState {
    pub fn key(self) -> &'static str {
        match self {
            SigState::Unknown => "unknown",
            SigState::Signed => "signed",
            SigState::Catalog => "catalog",
            SigState::Invalid => "invalid",
            SigState::Unsigned => "unsigned",
        }
    }
    pub fn parse(s: &str) -> SigState {
        match s {
            "signed" => SigState::Signed,
            "catalog" => SigState::Catalog,
            "invalid" => SigState::Invalid,
            "unsigned" => SigState::Unsigned,
            _ => SigState::Unknown,
        }
    }
    pub fn is_trusted(self) -> bool {
        matches!(self, SigState::Signed | SigState::Catalog)
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Event {
    pub id: u64,
    /// unix seconds
    pub time: u64,
    pub kind: Option<Kind>,
    /// service display name / exe name / app using the camera
    pub name: String,
    /// service key name, or app user model / package (may be empty)
    pub key: String,
    /// binary path
    pub path: String,
    /// "auto start → demand start", "kernel driver · boot", "used 7 min", version change...
    pub detail: String,
    /// "Installed by" heuristic, e.g. "GoogleUpdater.exe" or "svchost.exe → DrvInst.exe"
    pub actor: String,
    /// true when the actor is a guess (shown as "likely")
    pub actor_likely: bool,
    pub publisher: String,
    pub sig: SigState,
    /// for camera/mic/location: start of use (unix s)
    pub since: u64,
    pub unread: bool,
}

impl Event {
    pub fn kind(&self) -> Kind {
        self.kind.unwrap_or(Kind::NewApp)
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\t' => o.push_str("\\t"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('t') => o.push('\t'),
                Some('n') => o.push('\n'),
                Some('r') => o.push('\r'),
                Some(x) => o.push(x),
                None => {}
            }
        } else {
            o.push(c);
        }
    }
    o
}

pub fn encode(e: &Event) -> String {
    format!(
        "E\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        e.id,
        e.time,
        e.kind().key(),
        e.sig.key(),
        if e.unread { 1 } else { 0 },
        if e.actor_likely { 1 } else { 0 },
        e.since,
        esc(&e.name),
        esc(&e.key),
        esc(&e.path),
        esc(&e.detail),
        esc(&e.actor),
        esc(&e.publisher),
        "",
    )
}

pub fn decode(line: &str) -> Option<Event> {
    let f: Vec<&str> = line.trim_end_matches(['\r', '\n']).split('\t').collect();
    if f.len() < 14 || f[0] != "E" {
        return None;
    }
    Some(Event {
        id: f[1].parse().ok()?,
        time: f[2].parse().ok()?,
        kind: Some(Kind::parse(f[3])?),
        sig: SigState::parse(f[4]),
        unread: f[5] == "1",
        actor_likely: f[6] == "1",
        since: f[7].parse().unwrap_or(0),
        name: unesc(f[8]),
        key: unesc(f[9]),
        path: unesc(f[10]),
        detail: unesc(f[11]),
        actor: unesc(f[12]),
        publisher: unesc(f[13]),
    })
}

pub struct Store {
    file: PathBuf,
    ring: VecDeque<Event>,
    next_id: u64,
    /// lines in the file since the last compaction (events + markers)
    lines: usize,
}

impl Store {
    /// Load, prune and compact if needed. `now` = unix seconds.
    pub fn open(file: PathBuf, now: u64) -> Store {
        // pass 1: count, read markers, newest RAM window
        let mut total = 0usize;
        let mut lines = 0usize;
        let mut oldest = u64::MAX;
        let mut read: HashSet<u64> = HashSet::new();
        let mut all_read_upto = 0u64;
        // later text of an event (`U` lines: a network incident is closed with its end time)
        let mut upd: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        let mut max_id = 0u64;
        let mut ring: VecDeque<Event> = VecDeque::with_capacity(RAM_EVENTS + 1);
        if let Ok(f) = std::fs::File::open(&file) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                lines += 1;
                if let Some(e) = decode(&line) {
                    total += 1;
                    oldest = oldest.min(e.time);
                    max_id = max_id.max(e.id);
                    ring.push_back(e);
                    if ring.len() > RAM_EVENTS {
                        ring.pop_front();
                    }
                } else if let Some(id) = line.strip_prefix("R\t").and_then(|x| x.trim().parse().ok()) {
                    read.insert(id);
                } else if let Some(id) = line.strip_prefix("A\t").and_then(|x| x.trim().parse::<u64>().ok()) {
                    all_read_upto = all_read_upto.max(id);
                } else if let Some((id, d)) = line.strip_prefix("U\t").and_then(|x| x.split_once('\t')).and_then(|(id, d)| Some((id.parse::<u64>().ok()?, unesc(d.trim_end_matches(['\r', '\n']))))) {
                    upd.insert(id, d);
                }
            }
        }
        let cutoff = now.saturating_sub(MAX_AGE_SECS);
        let is_read = |e: &Event| e.id <= all_read_upto || read.contains(&e.id);
        for e in ring.iter_mut() {
            if is_read(e) {
                e.unread = false;
            }
            if let Some(d) = upd.get(&e.id) {
                e.detail = d.clone();
            }
        }
        ring.retain(|e| e.time >= cutoff);
        let markers = lines - total;
        let mut s = Store { file, ring, next_id: max_id + 1, lines };
        if total > MAX_EVENTS || (total > 0 && oldest < cutoff) || markers > 2000 {
            s.compact(total, cutoff, &read, all_read_upto, &upd);
        }
        s
    }

    /// Rewrite the file keeping the newest MAX_EVENTS within MAX_AGE, read flags inline.
    fn compact(&mut self, total: usize, cutoff: u64, read: &HashSet<u64>, all_read_upto: u64, upd: &std::collections::HashMap<u64, String>) {
        let skip = total.saturating_sub(MAX_EVENTS);
        let mut out = String::new();
        let mut n = 0usize;
        let mut kept = 0usize;
        if let Ok(f) = std::fs::File::open(&self.file) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if let Some(mut e) = decode(&line) {
                    n += 1;
                    if n <= skip || e.time < cutoff {
                        continue;
                    }
                    if e.id <= all_read_upto || read.contains(&e.id) {
                        e.unread = false;
                    }
                    if let Some(d) = upd.get(&e.id) {
                        e.detail = d.clone();
                    }
                    out.push_str(&encode(&e));
                    kept += 1;
                }
            }
        }
        if crate::osutil::write_atomic(&self.file, out.as_bytes()).is_ok() {
            self.lines = kept;
        }
    }

    /// Appended by the I/O worker, in order: the store lock (read by every
    /// paint for the unread badge) is never held across disk I/O.
    fn append(&mut self, s: &str) {
        crate::bgio::append(self.file.clone(), s.to_string());
        self.lines += 1;
    }

    /// Store a new event (id assigned, unread). Returns the id.
    pub fn push(&mut self, e: Event) -> u64 {
        self.push_as(e, true)
    }

    /// Store a new event with an explicit read flag (old caught-up events
    /// are logged as read so they don't flood the unread badge).
    pub fn push_as(&mut self, mut e: Event, unread: bool) -> u64 {
        e.id = self.next_id;
        self.next_id += 1;
        e.unread = unread;
        self.append(&encode(&e));
        let id = e.id;
        self.ring.push_back(e);
        if self.ring.len() > RAM_EVENTS {
            self.ring.pop_front();
        }
        id
    }

    /// Replace the detail line of a stored event (a network incident gets its end time). Persisted as
    /// an appended `U` line; the event keeps its id, time and read state.
    pub fn update_detail(&mut self, id: u64, detail: &str) -> bool {
        let in_ring = if let Some(e) = self.ring.iter_mut().find(|e| e.id == id) {
            e.detail = detail.to_string();
            true
        } else {
            false
        };
        // The close line goes to disk even when the event has aged out of the RAM ring (a long
        // incident): otherwise it would stay open in events.log. Ids never reached are refused.
        if in_ring || (id > 0 && id < self.next_id) {
            self.append(&format!("U	{}	{}
", id, esc(detail)));
        }
        in_ring
    }

    pub fn mark_read(&mut self, id: u64) {
        if let Some(e) = self.ring.iter_mut().find(|e| e.id == id && e.unread) {
            e.unread = false;
            self.append(&format!("R\t{}\n", id));
        }
    }

    pub fn mark_all_read(&mut self) {
        if self.ring.iter().any(|e| e.unread) {
            for e in self.ring.iter_mut() {
                e.unread = false;
            }
            self.append(&format!("A\t{}\n", self.next_id - 1));
        }
    }

    /// Newest first.
    pub fn iter(&self) -> impl Iterator<Item = &Event> {
        self.ring.iter().rev()
    }

    pub fn get(&self, id: u64) -> Option<&Event> {
        self.ring.iter().find(|e| e.id == id)
    }

    pub fn unread(&self) -> usize {
        self.ring.iter().filter(|e| e.unread).count()
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("blink-store-{}-{}", std::process::id(), name));
        let _ = std::fs::create_dir_all(&d);
        let f = d.join("events.log");
        let _ = std::fs::remove_file(&f);
        f
    }

    fn ev(time: u64, name: &str) -> Event {
        Event {
            time,
            kind: Some(Kind::DriverInstalled),
            name: name.into(),
            key: "NEPb67ocmj".into(),
            path: "C:\\Windows\\System32\\drivers\\x\ty.sys".into(),
            detail: "kernel driver\nboot".into(),
            actor: "svchost.exe → DrvInst.exe".into(),
            actor_likely: true,
            publisher: "Contoso \\ Ltd".into(),
            sig: SigState::Invalid,
            since: 5,
            ..Default::default()
        }
    }

    #[test]
    fn line_roundtrip() {
        let mut e = ev(100, "a");
        e.id = 9;
        e.unread = true;
        assert_eq!(decode(&encode(&e)).unwrap(), e);
    }

    #[test]
    fn store_roundtrip_and_read_flags() {
        let f = tmp("rt");
        let now = 1_800_000_000;
        let mut s = Store::open(f.clone(), now);
        let a = s.push(ev(now - 10, "a"));
        let b = s.push(ev(now - 5, "b"));
        let c = s.push(ev(now - 1, "c"));
        s.mark_read(b);
        assert_eq!(s.unread(), 2);
        let s2 = Store::open(f.clone(), now);
        let v: Vec<_> = s2.iter().map(|e| (e.id, e.name.clone(), e.unread)).collect();
        assert_eq!(v, vec![(c, "c".into(), true), (b, "b".into(), false), (a, "a".into(), true)]);
        let mut s2 = s2;
        s2.mark_all_read();
        let s3 = Store::open(f, now);
        assert_eq!(s3.unread(), 0);
        assert_eq!(s3.get(a).unwrap().detail, "kernel driver\nboot");
    }

    #[test]
    fn a_close_line_is_appended_even_when_the_event_left_the_ram_ring() {
        let f = tmp("upd-old");
        let mut s = Store::open(f, 1_800_000_000);
        let id = s.push(Event { time: 1_800_000_000, kind: Some(Kind::Network), ..Default::default() });
        for i in 0..(RAM_EVENTS + 5) {
            s.push(ev(1_800_000_000, &format!("n{i}")));
        }
        assert!(s.get(id).is_none(), "aged out of the ring");
        let before = s.lines;
        assert!(!s.update_detail(id, "closed"), "nothing in RAM to update");
        assert_eq!(s.lines, before + 1, "but the U line was still written");
        let before = s.lines;
        s.update_detail(u64::MAX, "never issued");
        assert_eq!(s.lines, before, "an id never issued writes nothing");
    }

    #[test]
    fn a_network_incident_is_closed_by_an_update_line_that_survives_reopen_and_compaction() {
        let f = tmp("upd");
        let now = 1_800_000_000;
        let mut s = Store::open(f.clone(), now);
        let e = Event { time: now - 60, kind: Some(Kind::Network), name: "x.exe".into(), detail: "could not reach 10.0.0.2:80 · 61 failed/min · since 14:02".into(), ..Default::default() };
        let id = s.push(e);
        assert!(s.update_detail(id, "could not reach 10.0.0.2:80 · 61 failed/min · 14:02–14:09"));
        assert!(!s.update_detail(999, "nobody"), "unknown id");
        assert_eq!(s.get(id).unwrap().detail, "could not reach 10.0.0.2:80 · 61 failed/min · 14:02–14:09");
        // the append goes through the I/O worker; write the same lines directly for the reopen check
        let text = format!("{}U\t{}\t{}\n", encode(s.get(id).map(|e| Event { detail: "open".into(), ..e.clone() }).as_ref().unwrap()), id, "closed\\nline");
        std::fs::write(&f, text).unwrap();
        let s2 = Store::open(f.clone(), now);
        assert_eq!(s2.get(id).unwrap().detail, "closed\nline", "the U line wins, and its escapes are undone");
        assert_eq!(s2.get(id).unwrap().kind(), Kind::Network);
        // compaction keeps the closed text
        let old = Event { time: now - MAX_AGE_SECS - 5, kind: Some(Kind::Network), name: "old".into(), ..Default::default() };
        let text = format!("{}{}U\t{}\t{}\n", encode(&Event { id: 1, ..old }), encode(&Event { id: 2, time: now - 10, kind: Some(Kind::Network), detail: "open".into(), ..Default::default() }), 2, "closed");
        std::fs::write(&f, text).unwrap();
        let s3 = Store::open(f.clone(), now);
        assert_eq!(s3.get(2).unwrap().detail, "closed");
        let on_disk = std::fs::read_to_string(&f).unwrap();
        assert!(on_disk.contains("\tclosed\t") && !on_disk.contains("\topen\t"), "{on_disk}");
    }

    #[test]
    fn prune_old_and_compact() {
        let f = tmp("prune");
        let now = 1_800_000_000;
        let mut s = Store::open(f.clone(), now);
        s.push(ev(now - MAX_AGE_SECS - 100, "old"));
        let keep = s.push(ev(now - 100, "new"));
        s.mark_read(keep);
        let s2 = Store::open(f.clone(), now);
        assert_eq!(s2.len(), 1);
        assert_eq!(s2.get(keep).unwrap().unread, false);
        let text = std::fs::read_to_string(&f).unwrap();
        assert_eq!(text.lines().count(), 1, "compacted: {}", text);
        // ids keep growing after compaction
        let mut s2 = s2;
        assert!(s2.push(ev(now, "x")) > keep);
    }
}
