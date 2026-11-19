#![windows_subsystem = "windows"]

mod bgio;
mod etw;
mod features;
mod gfx;
mod history;
mod trayicon;
mod leak;
#[cfg(debug_assertions)]
mod memreport;
mod netconn;
mod netetw;
mod netincident;
mod nettest;
mod netscope;
mod notify;
mod osutil;
mod power;
mod rules;
mod sample;
mod sensors;
mod setup;
mod startup;
mod svcctl;
mod taskbar;
mod taskmgr;
mod timing;
mod ui;
mod update;
mod watch;
mod win;
mod winplace;
mod zoom;

fn main() {
    // started as blink-tm.exe (the Task Manager redirect's launcher): do that one job and exit,
    // before anything else of Blink starts
    if taskmgr::launcher() {
        return;
    }
    // elevated helper of the Settings switch (--taskmgr-enable / --taskmgr-disable): no window
    if taskmgr::cli() {
        return;
    }
    // an elevated uninstall / all-users install never takes its data dir from the caller
    {
        let a: Vec<String> = std::env::args().skip(1).collect();
        if osutil::is_elevated() && (setup::logic::is_uninstall_launch(&a) || setup::logic::parse_install_machine(&a).is_some()) {
            osutil::lock_data_dir_to_known_folder();
        }
    }
    // before anything reads BLINK_DATA_DIR / BLINK_MULTI (UAC update relaunch)
    update::env_from_args();
    timing::mark("main");
    // windows subsystem has no console: keep panics in %LOCALAPPDATA%\Blink\panic.log
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        let msg = format!("{info}\n{bt}\n");
        let p = osutil::data_dir().join("panic.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) { use std::io::Write; let _ = f.write_all(msg.as_bytes()); }
    }));
    // `--startup-release` (install.ps1 -Uninstall): give back the Windows startup entries Blink switched off, no window; exit 1 = nothing may be removed
    if startup::release_cli() {
        return;
    }
    // `--remove-jumplist` (install.ps1 -Uninstall): delete the taskbar Jump List, no window
    if taskbar::remove_cli() {
        return;
    }
    // `--uninstall` (Settings > Apps > Installed apps): two native boxes, then the normal uninstall; no window
    if setup::install_machine_cli() {
        return;
    }
    if setup::uninstall_cli() {
        return;
    }
    // unelevated UAC helper of an elevated self-update: no window, exits
    if update::helper() {
        return;
    }
    // Debug builds only: exercise the unelevated launcher headlessly
    // (an elevated Blink window cannot be driven by test tools: UIPI).
    #[cfg(debug_assertions)]
    {
        let a: Vec<String> = std::env::args().collect();
        if a.len() == 3 && a[1] == "--test-launch-unelevated" {
            let r = rules::actions::launch_unelevated(&a[2], None);
            osutil::log(&format!("test-launch-unelevated elevated={} {:?} -> {r:?}", osutil::is_elevated(), a[2]));
            return;
        }
        if a.len() == 4 && a[1] == "--test-svc" {
            // debug test hook: only ever acts on a disposable fixture service, never a real one
            if !a[2].to_lowercase().starts_with("blinkfixture") {
                osutil::log(&format!("test-svc refused: {} is not a BlinkFixture service", a[2]));
                return;
            }
            // the fixture itself stands in for the list row: a complete baseline is read once, here, up front
            let t = match svcctl::debug_baseline(&a[2]) {
                Ok(t) => t,
                Err(e) => {
                    osutil::log(&format!("test-svc {} could not be read: {e}", a[2]));
                    return;
                }
            };
            match a[3].as_str() {
                // startup type: --test-svc <fixture> auto|delayed|manual|disabled
                k @ ("auto" | "delayed" | "manual" | "disabled") => {
                    let want = match k { "auto" => svcctl::StartKind::Automatic, "delayed" => svcctl::StartKind::AutomaticDelayed, "manual" => svcctl::StartKind::Manual, _ => svcctl::StartKind::Disabled };
                    let r = match t.kind() {
                        Some(expect) => ui::services::set_startup_wait(t, expect, want),
                        None => "its start type cannot be changed here".to_string(),
                    };
                    osutil::log(&format!("test-svc {} {} elevated={} -> {r}", a[2], a[3], osutil::is_elevated()));
                }
                other => {
                    let op = match other { "start" => svcctl::Op::Start, "stop" => svcctl::Op::Stop, _ => svcctl::Op::Restart };
                    ui::services::run(t, op);
                    std::thread::sleep(std::time::Duration::from_secs(20));
                    osutil::log(&format!("test-svc {} {} elevated={} -> {}", a[2], a[3], osutil::is_elevated(), ui::services::last_message()));
                }
            }
            return;
        }
        if a.len() == 2 && a[1] == "--test-cpu-temp" {
            for _ in 0..3 {
                let r = match sensors::cpu_temp() {
                    sensors::CpuRead::Ok(t) => format!("ok {t:.1} C"),
                    sensors::CpuRead::NotInstalled => "not installed".into(),
                    sensors::CpuRead::NeedsAdmin => "needs admin".into(),
                    sensors::CpuRead::Failed => "failed".into(),
                };
                osutil::log(&format!("test-cpu-temp elevated={} {r}", osutil::is_elevated()));
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            return;
        }
        if a.len() == 2 && a[1] == "--test-gpu" {
            let mut g = sample::gpu::Gpus::load();
            g.sample(false);
            std::thread::sleep(std::time::Duration::from_secs(1));
            for x in g.sample(false) {
                osutil::log(&format!(
                    "test-gpu {} util={:.0}% temp={:?} vram={:.2}/{:.1} GB power={:?}% clock={:?} MHz fan={:?} rpm",
                    x.name, x.util, x.temp, x.vram_used as f64 / 1073741824.0, x.vram_total as f64 / 1073741824.0, x.power_pct, x.clock_mhz, x.fan_rpm
                ));
            }
            return;
        }
    }
    #[cfg(debug_assertions)]
    memreport::start();
    // -ToastActivated (COM, notification-centre click): which Watch event to open
    setup::early();
    update::early();
    win::run();
    // an uninstall's Jump List removal still waiting on the shell gets a little longer
    taskbar::drain(std::time::Duration::from_secs(3));
    // a staged update closed the window: relaunch the new build, watch its start
    update::finish();
    // self-install / uninstall closed the window: start the installed copy / clean up
    setup::finish();
}
