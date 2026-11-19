//! Startup management: enumerate every Windows startup entry, enable/disable
//! ordinary ones, and (opt-in) run a user-managed list at logon.
//!
//! Layout: `id` (what an entry IS), `reg` / `approved` (registry + the
//! StartupApproved state), `scan` (enumeration, read-only). Later: the managed
//! store, adoption, the scheduler and the launcher.

pub mod adopt;
pub mod approved;
pub mod id;
pub mod import;
pub mod launch;
pub mod model;
pub mod own;
pub mod proof;
pub mod reg;
pub mod run;
pub mod scan;
pub mod sched;
pub mod store;

/// `hayate.exe --startup-release` (install.ps1 -Uninstall, headless): give every
/// Windows startup entry Hayate switched off back. Exit code 0 = everything Hayate
/// owned is back (or nothing was owned); 1 = something could not be given back, and
/// the uninstaller must remove NOTHING. The result goes to the log; there is no console.
pub const ARG_RELEASE: &str = "--startup-release";

pub fn release_cli() -> bool {
    if !std::env::args().skip(1).any(|a| a == ARG_RELEASE) {
        return false;
    }
    match run::release_all_owned() {
        Ok(r) => {
            crate::osutil::log(&format!("startup: release for uninstall: {}", r.summary()));
            if !r.left.is_empty() {
                crate::osutil::log(&format!("startup: release for uninstall: left: {}", r.left.join("; ")));
            }
            for w in &r.skipped {
                crate::osutil::log(&format!("startup: release for uninstall: WARNING {w}"));
            }
            true
        }
        Err(e) => {
            crate::osutil::log(&format!("startup: release for uninstall FAILED: {e}"));
            std::process::exit(1);
        }
    }
}
