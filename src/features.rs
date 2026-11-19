//! One entry point for the resident Rules + Watch backend. The window code
//! calls `start` once (from win::run) and keeps the returned handles.
#![allow(dead_code)]

use std::sync::Arc;

use crate::{etw, rules, watch};

pub struct Features {
    pub rules: Arc<rules::Engine>,
    pub watch: Arc<watch::Watch>,
    /// Etw, or Snapshot when the ETW session could not start (see etw::status()).
    pub source: etw::Source,
}

/// Start everything. `on_toast` gets each (throttled, coalesced) toast to show
/// (Windows toast or tray balloon, see `notify`); `on_change` fires when Watch events change (repaint).
/// Both run on background threads: post a message to the UI thread.
pub fn start(on_toast: impl Fn(watch::Toast) + Send + Sync + 'static, on_change: impl Fn() + Send + Sync + 'static) -> Features {
    // subscribers first, so no start is missed
    use crate::timing::time;
    let rules = time("rules start", rules::start);
    let watch = time("watch start", watch::start);
    watch.set_toast_sink(on_toast);
    watch.set_on_change(on_change);
    let source = time("etw start", etw::start);
    Features { rules, watch, source }
}

static FEAT: std::sync::OnceLock<Features> = std::sync::OnceLock::new();

/// Start once and keep globally (UI thread + tray read it).
pub fn init(on_toast: impl Fn(watch::Toast) + Send + Sync + 'static, on_change: impl Fn() + Send + Sync + 'static) -> &'static Features {
    FEAT.get_or_init(|| start(on_toast, on_change))
}

pub fn get() -> Option<&'static Features> {
    FEAT.get()
}

impl Features {
    /// Restore power plan, release sleep requests, save bookmark/stats, stop ETW.
    pub fn shutdown(&self) {
        self.rules.shutdown();
        self.watch.shutdown();
        etw::stop();
        crate::netetw::shutdown();
        // queued settings / event writes reach the disk before the process ends
        crate::bgio::flush(std::time::Duration::from_millis(500));
    }
}
