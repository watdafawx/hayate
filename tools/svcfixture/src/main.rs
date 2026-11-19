//! A service that does nothing: reports Running, accepts Stop, exits on Stop.
//! Register it under a `HayateFixture...` name on a test machine only (see
//! docs/services.md); Hayate's debug test hooks and the ignored real-SCM test
//! refuse any other name.
//!
//! The name it answers to is whatever the SCM started it as: ServiceMain's first
//! argument is the service name (winsvc.h, LPSERVICE_MAIN_FUNCTION), and that is
//! what it registers its control handler under. The dispatcher table name is
//! ignored for a SERVICE_WIN32_OWN_PROCESS service (learn.microsoft.com,
//! SERVICE_TABLE_ENTRYW), so none is hard-coded. The fixture must be registered
//! as own-process (`type= own`).

use std::sync::atomic::{AtomicIsize, Ordering};
use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HANDLE, NO_ERROR};
use windows::Win32::System::Services::*;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject, INFINITE};

static STATUS: AtomicIsize = AtomicIsize::new(0);
static STOP_EVENT: AtomicIsize = AtomicIsize::new(0);

fn report(state: SERVICE_STATUS_CURRENT_STATE, accepts: u32) {
    let st = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accepts,
        dwWin32ExitCode: NO_ERROR.0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: 0,
        dwWaitHint: 3000,
    };
    unsafe {
        let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(STATUS.load(Ordering::SeqCst) as *mut _), &st);
    }
}

unsafe extern "system" fn handler(control: u32, _ev: u32, _data: *mut core::ffi::c_void, _ctx: *mut core::ffi::c_void) -> u32 {
    if control == SERVICE_CONTROL_STOP {
        report(SERVICE_STOP_PENDING, 0);
        let _ = SetEvent(HANDLE(STOP_EVENT.load(Ordering::SeqCst) as *mut _));
    }
    NO_ERROR.0
}

unsafe extern "system" fn service_main(argc: u32, argv: *mut PWSTR) {
    // argv[0] is the name this service was started under; it registers under exactly that
    let name = if argc >= 1 && !argv.is_null() && !(*argv).is_null() { PCWSTR((*argv).0 as *const u16) } else { w!("HayateFixture") };
    let Ok(h) = RegisterServiceCtrlHandlerExW(name, Some(handler), None) else { return };
    STATUS.store(h.0 as isize, Ordering::SeqCst);
    let Ok(ev) = CreateEventW(None, true, false, None) else { return };
    STOP_EVENT.store(ev.0 as isize, Ordering::SeqCst);
    report(SERVICE_RUNNING, SERVICE_ACCEPT_STOP);
    WaitForSingleObject(ev, INFINITE);
    report(SERVICE_STOPPED, 0);
}

fn main() {
    let table = [
        // ignored for an own-process service, but must not be NULL: an empty string
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(w!("").as_ptr() as *mut _), lpServiceProc: Some(service_main) },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe {
        let _ = StartServiceCtrlDispatcherW(table.as_ptr());
    }
}
