//! Sensors data: PawnIO-backed CPU package temperature (AMD Zen "Tctl/Tdie")
//! plus the pure register decoding used to interpret it. GPU and storage
//! sensors live in `sample` (D3DKMT / disk) and are reused by the Sensors panel
//! of the Vitals detail views,
//! so this module only handles what the sampler cannot: the CPU's on-die SMU
//! thermal register, read through PawnIO's signed `AMDFamily17` module.
//!
//! PawnIO is loaded at runtime with LoadLibraryW/GetProcAddress so Blink starts
//! and runs fine when the driver is absent. Blink never installs, starts or
//! otherwise touches the PawnIO service; if `PawnIOLib.dll` is missing or the
//! service is not running, `cpu_temp` reports `CpuRead::NotInstalled` and the
//! Vitals CPU detail shows "PawnIO not installed" while the GPU / storage panels keep
//! working.
//!
//! Provenance of the embedded module (see docs/sensors.md):
//!   AMDFamily17.bin  PawnIO.Modules release 0.2.11 (tag `0.2.11`)
//!   sha256 dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91

use std::sync::OnceLock;

use windows::core::{w, PCSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, WAIT_OBJECT_0};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{OpenMutexW, ReleaseMutex, WaitForSingleObject, SYNCHRONIZATION_SYNCHRONIZE};

/// Signed PawnIO module binary (official release, sha256 verified — above).
const AMDFAMILY17: &[u8] = include_bytes!("../res/pawnio/AMDFamily17.bin");

/// SMN address of THM_TCON_CUR_TMP — the Zen (families 17h–1Ah) package
/// temperature register. Value layout: CUR_TEMP[31:21], RANGE_SEL[19],
/// TJ_SEL[17:16].
const THM_TCON_CUR_TMP: u64 = 0x0005_9800;

// PawnIOLib entry points. STDAPICALLTYPE == `system` on both x86 and x64.
type FOpen = unsafe extern "system" fn(*mut HANDLE) -> windows::core::HRESULT;
type FLoad = unsafe extern "system" fn(HANDLE, *const u8, usize) -> windows::core::HRESULT;
type FExec = unsafe extern "system" fn(HANDLE, PCSTR, *const u64, usize, *mut u64, usize, *mut usize) -> windows::core::HRESULT;
type FClose = unsafe extern "system" fn(HANDLE) -> windows::core::HRESULT;

/// Loaded PawnIOLib module base + resolved entry points. The DLL is loaded once
/// and held for the process lifetime (the executor handle itself is still
/// opened/closed per read, so nothing is held while the window is hidden).
struct Api {
    open: FOpen,
    load: FLoad,
    exec: FExec,
    close: FClose,
}

// `Api` is immutable (a module base + four function pointers) and its DLL is
// never unloaded, so sharing it across threads is sound.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Api> = OnceLock::new();

unsafe fn proc_addr(lib: HMODULE, name: PCSTR) -> Option<usize> {
    GetProcAddress(lib, name).map(|p| p as usize)
}

/// Resolve the PawnIOLib exports once, or `None` when the DLL is not installed.
fn api() -> Option<&'static Api> {
    if let Some(a) = API.get() {
        return Some(a);
    }
    // not cached on failure, so a fresh PawnIO install is picked up without a restart
    let a = unsafe {
        // installed under Program Files, which is not on the DLL search path
        // known-folder API, never %ProgramFiles%: the environment is the caller's to write
        let pf = crate::setup::program_files().unwrap_or_else(|| r"C:\Program Files".into());
        let full: Vec<u16> = format!(r"{pf}\PawnIO\PawnIOLib.dll").encode_utf16().chain(Some(0)).collect();
        let lib = LoadLibraryW(windows::core::PCWSTR(full.as_ptr())).ok()?;
        let open = std::mem::transmute::<usize, FOpen>(proc_addr(lib, PCSTR(c"pawnio_open".as_ptr().cast()))?);
        let load = std::mem::transmute::<usize, FLoad>(proc_addr(lib, PCSTR(c"pawnio_load".as_ptr().cast()))?);
        let exec = std::mem::transmute::<usize, FExec>(proc_addr(lib, PCSTR(c"pawnio_execute".as_ptr().cast()))?);
        let close = std::mem::transmute::<usize, FClose>(proc_addr(lib, PCSTR(c"pawnio_close".as_ptr().cast()))?);
        Api { open, load, exec, close }
    };
    let _ = API.set(a);
    API.get()
}

/// Result of one CPU temperature read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CpuRead {
    /// A plausible package temperature in °C.
    Ok(f32),
    /// PawnIOLib.dll is missing or the PawnIO service is not running.
    NotInstalled,
    /// Installed, but Blink is not running elevated.
    NeedsAdmin,
    /// The driver is there but the module load or SMN read failed (e.g. a
    /// non-AMD CPU, a torn read, or a transient driver error).
    Failed,
}

/// Read the package temperature once (on the I/O worker: a driver call). Opens and closes a fresh PawnIO executor
/// inside this call, so no handle is ever held while the window is hidden.
pub fn cpu_temp() -> CpuRead {
    crate::bgio::fake_stall("pawnio");
    let Some(api) = api() else {
        return CpuRead::NotInstalled;
    };
    unsafe {
        let mut h = HANDLE::default();
        let r = (api.open)(&mut h);
        if r.is_err() {
            static LOGGED: std::sync::Once = std::sync::Once::new();
            LOGGED.call_once(|| crate::osutil::log(&format!("pawnio_open failed: {r:?}")));
            // E_ACCESSDENIED: the PawnIO device only opens for administrators
            return if r == windows::core::HRESULT(0x80070005u32 as i32) { CpuRead::NeedsAdmin } else { CpuRead::NotInstalled };
        }
        if (api.load)(h, AMDFAMILY17.as_ptr(), AMDFAMILY17.len()).is_err() {
            let _ = (api.close)(h);
            return CpuRead::Failed;
        }
        // Serialize the SMN/PCI-config read with other monitoring tools that
        // own `\BaseNamedObjects\Access_PCI` (HWiNFO, LibreHardwareMonitor).
        let _guard = pci_mutex();
        let input = [THM_TCON_CUR_TMP];
        let mut out = [0u64; 1];
        let mut n = 0usize;
        let hr = (api.exec)(h, PCSTR(c"ioctl_read_smn".as_ptr().cast()), input.as_ptr(), 1, out.as_mut_ptr(), 1, &mut n);
        let _ = (api.close)(h);
        if hr.is_err() || n < 1 {
            return CpuRead::Failed;
        }
        let t = decode_tctl(out[0] as u32);
        // A plausible package temperature; anything else is a torn read (a
        // race with another tool, or an all-ones bus value).
        if (-20.0..=130.0).contains(&t) {
            CpuRead::Ok(t)
        } else {
            CpuRead::Failed
        }
    }
}

/// Decode THM_TCON_CUR_TMP into degrees Celsius.
///
/// `CUR_TEMP[31:21]` is the temperature in 0.125 °C steps. Zen parts report a
/// 49 °C offset when `RANGE_SEL[19]` is set, or when `TJ_SEL[17:16] == 0b11`
/// (newer parts signal it either way). A 5900X has no further Tctl offset, so
/// Tctl == Tdie.
pub fn decode_tctl(raw: u32) -> f32 {
    const RANGE_SEL: u32 = 0x8_0000;
    const TJ_SEL: u32 = 0x3_0000;
    let offset = (raw & RANGE_SEL) != 0 || (raw & TJ_SEL) == TJ_SEL;
    let t = (raw >> 21) as f32 * 0.125;
    if offset { t - 49.0 } else { t }
}

/// Holds the `\BaseNamedObjects\Access_PCI` mutant for the duration of one SMN
/// read, releasing it on drop.
struct PciMutex(HANDLE);
impl Drop for PciMutex {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// Best-effort serialization with other monitoring tools. Returns `None` when
/// the mutant does not exist (no other tool is running) or is held too long.
fn pci_mutex() -> Option<PciMutex> {
    unsafe {
        let h = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, w!("\\BaseNamedObjects\\Access_PCI")).ok()?;
        if WaitForSingleObject(h, 10) == WAIT_OBJECT_0 {
            Some(PciMutex(h))
        } else {
            let _ = CloseHandle(h);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::decode_tctl;

    #[test]
    fn tctl_no_offset() {
        // CUR_TEMP = 400 (0x190) in 0.125 °C steps => 50.0 °C, no offset bits.
        assert_eq!(decode_tctl(0x190 << 21), 50.0);
    }

    #[test]
    fn tctl_range_sel_offsets_by_49() {
        assert_eq!(decode_tctl((0x190 << 21) | 0x8_0000), 1.0);
    }

    #[test]
    fn tctl_tj_sel_offsets_by_49() {
        assert_eq!(decode_tctl((0x190 << 21) | 0x3_0000), 1.0);
    }

    #[test]
    fn tctl_zero_shift_is_zero() {
        assert_eq!(decode_tctl(0), 0.0);
    }
}
