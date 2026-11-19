//! Toast queue: per-type alert routing, 1 toast per type per 10 s with
//! coalescing ("3 services installed"), do-not-disturb. The parent shows each
//! `Toast` as a Windows toast, or a tray balloon without the Start-menu shortcut
//! (see `notify`).

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const WINDOW_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AlertType {
    ServiceInstalled,
    ServiceRemoved,
    StartTypeChanged,
    ServiceCrashed,
    /// 7036 started/stopped: log only by default
    ServiceStartStop,
    NewApp,
    AppUpdated,
    Unsigned,
    Camera,
    Microphone,
    Location,
    MemoryLeak,
    MemoryPressure,
}

impl AlertType {
    pub const ALL: [AlertType; 13] = [
        AlertType::ServiceInstalled,
        AlertType::ServiceRemoved,
        AlertType::StartTypeChanged,
        AlertType::ServiceCrashed,
        AlertType::ServiceStartStop,
        AlertType::NewApp,
        AlertType::AppUpdated,
        AlertType::Unsigned,
        AlertType::Camera,
        AlertType::Microphone,
        AlertType::Location,
        AlertType::MemoryLeak,
        AlertType::MemoryPressure,
    ];
    pub fn key(self) -> &'static str {
        match self {
            AlertType::ServiceInstalled => "service_installed",
            AlertType::ServiceRemoved => "service_removed",
            AlertType::StartTypeChanged => "start_type_changed",
            AlertType::ServiceCrashed => "service_crashed",
            AlertType::ServiceStartStop => "service_start_stop",
            AlertType::NewApp => "new_app",
            AlertType::AppUpdated => "app_updated",
            AlertType::Unsigned => "unsigned_app",
            AlertType::Camera => "camera",
            AlertType::Microphone => "microphone",
            AlertType::Location => "location",
            AlertType::MemoryLeak => "memory_leak",
            AlertType::MemoryPressure => "memory_pressure",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            AlertType::ServiceInstalled => "Service or driver installed",
            AlertType::ServiceRemoved => "Service or driver removed",
            AlertType::StartTypeChanged => "Start type changed",
            AlertType::ServiceCrashed => "Service crashed",
            AlertType::ServiceStartStop => "Service started / stopped",
            AlertType::NewApp => "New app launched",
            AlertType::AppUpdated => "App updated",
            AlertType::Unsigned => "Unsigned app launched",
            AlertType::Camera => "Camera in use",
            AlertType::Microphone => "Microphone in use",
            AlertType::Location => "Location in use",
            AlertType::MemoryLeak => "Memory leak suspected",
            AlertType::MemoryPressure => "Memory almost full",
        }
    }
    pub fn parse(s: &str) -> Option<AlertType> {
        AlertType::ALL.into_iter().find(|a| a.key() == s)
    }
    /// User decision: everything toasts except 7036 started/stopped.
    pub fn default_notify(self) -> bool {
        self != AlertType::ServiceStartStop
    }
    /// Plural noun for a coalesced title.
    fn plural(self, n: usize) -> String {
        match self {
            AlertType::ServiceInstalled => format!("{} services/drivers installed", n),
            AlertType::ServiceRemoved => format!("{} services/drivers removed", n),
            AlertType::StartTypeChanged => format!("{} service start types changed", n),
            AlertType::ServiceCrashed => format!("{} services crashed", n),
            AlertType::ServiceStartStop => format!("{} service state changes", n),
            AlertType::NewApp => format!("{} new apps launched", n),
            AlertType::AppUpdated => format!("{} apps updated", n),
            AlertType::Unsigned => format!("{} unsigned apps launched", n),
            AlertType::Camera => format!("{} apps using the camera", n),
            AlertType::Microphone => format!("{} apps using the microphone", n),
            AlertType::Location => format!("{} apps using location", n),
            AlertType::MemoryLeak => format!("{} apps may be leaking memory", n),
            AlertType::MemoryPressure => format!("{} memory warnings", n),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Toast {
    pub alert: AlertType,
    /// ≤ 63 chars (balloon limit)
    pub title: String,
    /// ≤ 255 chars
    pub text: String,
    /// event to open on click (newest one when coalesced)
    pub event_id: Option<u64>,
    /// how many events this toast stands for
    pub count: usize,
    /// subject name (used when coalescing)
    pub subject: String,
    pub actor: String,
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut o: String = s.chars().take(n.saturating_sub(1)).collect();
    o.push('…');
    o
}

impl Toast {
    pub fn new(alert: AlertType, title: &str, text: &str, subject: &str, actor: &str, event_id: Option<u64>) -> Toast {
        Toast { alert, title: clip(title, 63), text: clip(text, 255), event_id, count: 1, subject: subject.into(), actor: actor.into() }
    }
}

/// Merge queued toasts of one type into one.
pub fn coalesce(v: Vec<Toast>) -> Option<Toast> {
    if v.len() <= 1 {
        return v.into_iter().next();
    }
    let alert = v[0].alert;
    let n: usize = v.iter().map(|t| t.count).sum();
    let mut names: Vec<&str> = Vec::new();
    for t in &v {
        if !names.contains(&t.subject.as_str()) {
            names.push(&t.subject);
        }
    }
    let shown = names.iter().take(4).copied().collect::<Vec<_>>().join(", ");
    let more = if names.len() > 4 { format!(" +{}", names.len() - 4) } else { String::new() };
    let same_actor = !v[0].actor.is_empty() && v.iter().all(|t| t.actor == v[0].actor);
    let text = if same_actor { format!("{}{} · by {}", shown, more, v[0].actor) } else { format!("{}{}", shown, more) };
    Some(Toast {
        alert,
        title: clip(&alert.plural(n), 63),
        text: clip(&text, 255),
        event_id: v.last().and_then(|t| t.event_id),
        count: n,
        subject: String::new(),
        actor: if same_actor { v[0].actor.clone() } else { String::new() },
    })
}

/// Pure throttle (testable). Times in ms.
#[derive(Default)]
pub struct Throttle {
    last: HashMap<AlertType, u64>,
    pending: HashMap<AlertType, Vec<Toast>>,
}

impl Throttle {
    /// Returns the toast to show now, or None if it was queued.
    pub fn push(&mut self, now: u64, t: Toast) -> Option<Toast> {
        let a = t.alert;
        let free = self.last.get(&a).map_or(true, |&l| now.saturating_sub(l) >= WINDOW_MS);
        if free && !self.pending.contains_key(&a) {
            self.last.insert(a, now);
            Some(t)
        } else {
            self.pending.entry(a).or_default().push(t);
            None
        }
    }

    /// Coalesced toasts whose window has passed.
    pub fn due(&mut self, now: u64) -> Vec<Toast> {
        let ready: Vec<AlertType> = self.pending.keys().copied().filter(|a| self.last.get(a).map_or(true, |&l| now.saturating_sub(l) >= WINDOW_MS)).collect();
        let mut out = Vec::new();
        for a in ready {
            if let Some(t) = self.pending.remove(&a).and_then(coalesce) {
                self.last.insert(a, now);
                out.push(t);
            }
        }
        out
    }

    pub fn next_due(&self) -> Option<u64> {
        self.pending.keys().filter_map(|a| self.last.get(a).map(|l| l + WINDOW_MS)).min()
    }

    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

type Sink = Arc<dyn Fn(Toast) + Send + Sync>;

struct Shared {
    th: Throttle,
    sink: Option<Sink>,
    quit: bool,
}

/// Throttle + timer thread that flushes coalesced toasts.
pub struct Toaster {
    s: Arc<(Mutex<Shared>, Condvar)>,
}

fn now_ms() -> u64 {
    crate::osutil::now_ms()
}

impl Toaster {
    pub fn new() -> Toaster {
        let s = Arc::new((Mutex::new(Shared { th: Throttle::default(), sink: None, quit: false }), Condvar::new()));
        let s2 = s.clone();
        let _ = std::thread::Builder::new().name("hayate-toast".into()).stack_size(64 * 1024).spawn(move || {
            let (m, cv) = &*s2;
            let mut g = m.lock().unwrap();
            loop {
                if g.quit {
                    return;
                }
                let wait = match g.th.next_due() {
                    Some(t) => Duration::from_millis(t.saturating_sub(now_ms()).max(50)),
                    None => Duration::from_secs(3600),
                };
                g = cv.wait_timeout(g, wait).unwrap().0;
                let due = g.th.due(now_ms());
                if !due.is_empty() {
                    if let Some(sink) = g.sink.clone() {
                        drop(g);
                        for t in due {
                            sink(t);
                        }
                        g = m.lock().unwrap();
                    }
                }
            }
        });
        Toaster { s }
    }

    pub fn set_sink(&self, f: Sink) {
        self.s.0.lock().unwrap().sink = Some(f);
    }

    pub fn push(&self, t: Toast) {
        let (m, cv) = &*self.s;
        let mut g = m.lock().unwrap();
        let now_t = g.th.push(now_ms(), t);
        let sink = g.sink.clone();
        drop(g);
        cv.notify_one();
        if let (Some(t), Some(s)) = (now_t, sink) {
            s(t);
        }
    }

    /// Drop queued toasts (entering do-not-disturb).
    pub fn clear(&self) {
        self.s.0.lock().unwrap().th.clear();
    }

    pub fn stop(&self) {
        self.s.0.lock().unwrap().quit = true;
        self.s.1.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(a: AlertType, s: &str, actor: &str, id: u64) -> Toast {
        Toast::new(a, "Service installed", s, s, actor, Some(id))
    }

    #[test]
    fn throttle_and_coalesce() {
        let mut th = Throttle::default();
        let a = AlertType::ServiceInstalled;
        assert!(th.push(0, t(a, "svc1", "msiexec.exe", 1)).is_some());
        assert!(th.push(1000, t(a, "svc2", "msiexec.exe", 2)).is_none());
        assert!(th.push(2000, t(a, "svc3", "msiexec.exe", 3)).is_none());
        // other type is independent
        assert!(th.push(2000, t(AlertType::Camera, "zen.exe", "", 4)).is_some());
        assert_eq!(th.next_due(), Some(10_000));
        assert!(th.due(9_999).is_empty());
        let d = th.due(10_000);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].title, "2 services/drivers installed");
        assert_eq!(d[0].text, "svc2, svc3 · by msiexec.exe");
        assert_eq!(d[0].event_id, Some(3));
        assert_eq!(d[0].count, 2);
        // window restarts at flush time
        assert!(th.push(15_000, t(a, "svc4", "", 5)).is_none());
        assert!(th.push(20_000, t(a, "svc4", "", 6)).is_none());
        let d = th.due(20_000);
        assert_eq!(d[0].text, "svc4");
    }

    #[test]
    fn clip_limits() {
        let long = "x".repeat(300);
        let t = Toast::new(AlertType::NewApp, &long, &long, "a", "", None);
        assert_eq!(t.title.chars().count(), 63);
        assert_eq!(t.text.chars().count(), 255);
    }
}
