//! The toast activator: a COM local server for one class
//! (`logic::CLSID`). Windows starts `hayate.exe -ToastActivated` through the
//! HKCU `LocalServer32` key when a toast in the notification centre is
//! clicked, then calls `INotificationActivationCallback::Activate` with the
//! toast's `launch` string (`watch:<event id>`).
//!
//! Only a process started with `-ToastActivated` registers the class, for a
//! few seconds, on its own MTA thread; the running Hayate never does (an
//! elevated Hayate could not be reached by the shell anyway). main then
//! forwards the event to the running Hayate or starts Hayate on Watch.

use std::sync::mpsc::{channel, Sender};
use std::time::Duration;
use windows::core::*;
use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_NOINTERFACE};
use windows::Win32::System::Com::*;
use windows::Win32::UI::Notifications::*;

#[implement(INotificationActivationCallback)]
struct Activator(Sender<String>);

impl INotificationActivationCallback_Impl for Activator_Impl {
    fn Activate(&self, _aumid: &PCWSTR, args: &PCWSTR, _data: *const NOTIFICATION_USER_INPUT_DATA, _count: u32) -> Result<()> {
        let a = if args.is_null() { String::new() } else { unsafe { args.to_string() }.unwrap_or_default() };
        let _ = self.0.send(a);
        Ok(())
    }
}

#[implement(IClassFactory)]
struct Factory(Sender<String>);

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<'_, IUnknown>, iid: *const GUID, out: *mut *mut core::ffi::c_void) -> Result<()> {
        if out.is_null() {
            return Err(E_NOINTERFACE.into());
        }
        unsafe { *out = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let unk: IUnknown = Activator(self.0.clone()).into();
        unsafe { unk.query(iid, out).ok() }
    }
    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

/// Register the class, wait up to `wait` for Windows to call Activate, revoke.
/// Returns the toast's launch args (None: no activation arrived).
pub fn wait_activation(wait: Duration) -> Option<String> {
    let (tx, rx) = channel::<String>();
    let t = std::thread::Builder::new().name("toast-activator".into()).spawn(move || unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IClassFactory = Factory(tx).into();
        let clsid = GUID::from_u128(super::logic::CLSID);
        match CoRegisterClassObject(&clsid, &factory, CLSCTX_LOCAL_SERVER, REGCLS_SINGLEUSE) {
            Ok(cookie) => {
                let got = rx.recv_timeout(wait).ok();
                let _ = CoRevokeClassObject(cookie);
                got
            }
            Err(e) => {
                crate::osutil::log(&format!("toast activator: CoRegisterClassObject failed: {e}"));
                None
            }
        }
    });
    t.ok()?.join().ok().flatten()
}
