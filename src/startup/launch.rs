//! Turning a managed entry's command into a running program.
//!
//! Parsing is a port of Launchpad's (quoted path, unquoted path with spaces
//! ending in a program extension, existing bare file, `shell:` URI, Squirrel
//! `Update.exe --processStart`). Starting is NOT: Launchpad is always a normal
//! user process, Hayate can be elevated, so the way a program is started is a
//! policy decision made here, in one place:
//!
//! * not Admin -> the normal interactive user token, always
//!   (`rules::actions::launch_as_user`: Explorer when Hayate is elevated, the
//!   verified shell token as the only fallback, never CreateProcess with
//!   Hayate's own token);
//! * Admin -> explicit `runas`, only from a Hayate that is NOT already elevated
//!   (from an elevated Hayate `runas` would start elevated code from a per-user
//!   config file, so it is refused), and only from an explicit button
//!   (Run now / Run all now): never from the automatic logon run. Whether
//!   `runas` shows a UAC prompt is Windows policy (ConsentPromptBehaviorAdmin
//!   can make it silent), so Hayate does not promise a prompt.
//! * If Hayate cannot tell whether it is elevated, nothing is started.
//!
//! Nothing here builds a shell command: the program and its arguments are two
//! strings handed to the OS API.

use std::path::Path;

use windows::Win32::Foundation::ERROR_CANCELLED;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::osutil::{pcw, wide};

use super::scan::expand_env;

/// Extensions that end a program path (not an argument).
const PROGRAMS: [&str; 5] = [".exe", ".lnk", ".url", ".bat", ".cmd"];

/// Split a command line into (program, arguments). An UNQUOTED path with spaces
/// is only split where the text up to a program extension names an existing
/// file; if it does not, the first space is the end of the program (nothing is
/// guessed). See [`unambiguous`] for the case where that is not enough.
pub fn split_first(cmd: &str) -> (String, String) {
    let c = cmd.trim();
    if let Some(r) = c.strip_prefix('"') {
        return match r.find('"') {
            Some(i) => (r[..i].to_string(), r[i + 1..].trim().to_string()),
            None => (r.to_string(), String::new()),
        };
    }
    // no arguments: the whole line is the file, whatever its extension
    if Path::new(c).is_file() {
        return (c.to_string(), String::new());
    }
    let lower = c.to_ascii_lowercase();
    // the FIRST candidate end that is an existing file wins (a directory called `x.exe` is not one)
    let mut cands: Vec<usize> = Vec::new();
    for ext in PROGRAMS {
        let mut from = 0;
        while let Some(i) = lower[from..].find(ext) {
            let end = from + i + ext.len();
            if end == c.len() || c.as_bytes()[end] == b' ' {
                cands.push(end);
            }
            from = end;
        }
    }
    cands.sort_unstable();
    if let Some(end) = cands.iter().copied().find(|&e| Path::new(&c[..e]).is_file()) {
        return (c[..end].to_string(), c[end..].trim().to_string());
    }
    // nothing on disk to anchor on (a program that is not installed here, or a
    // bare name): the old rule, but only for the first candidate
    if let Some(end) = cands.first().copied() {
        return (c[..end].to_string(), c[end..].trim().to_string());
    }
    match c.find(' ') {
        Some(i) => (c[..i].to_string(), c[i + 1..].trim().to_string()),
        None => (c.to_string(), String::new()),
    }
}

/// An unquoted command whose program path contains spaces AND could end at more
/// than one program extension (`C:\Tools\a.exe\b.exe x`, where `a.exe` could be
/// a folder) is ambiguous: starting the wrong one would run a program the user
/// did not mean. Err = refuse, with the fix (quote the path).
pub fn unambiguous(cmd: &str) -> Result<(), String> {
    let c = cmd.trim();
    if c.starts_with('"') || Path::new(c).is_file() {
        return Ok(());
    }
    let lower = c.to_ascii_lowercase();
    let mut ends = Vec::new();
    for ext in PROGRAMS {
        let mut from = 0;
        while let Some(i) = lower[from..].find(ext) {
            let end = from + i + ext.len();
            if end == c.len() || c.as_bytes()[end] == b' ' {
                ends.push(end);
            }
            from = end;
        }
    }
    ends.sort_unstable();
    let on_disk = ends.iter().filter(|&&e| Path::new(&c[..e]).is_file()).count();
    if ends.len() > 1 && on_disk != 1 {
        return Err("this command is ambiguous: the program path has spaces and is not in quotes, and more than one part of it could be the program. Put the program path in quotes.".into());
    }
    Ok(())
}

fn arg_after<'a>(rest: &'a str, flag: &str) -> Option<&'a str> {
    let mut it = rest.split_whitespace();
    while let Some(a) = it.next() {
        if a.eq_ignore_ascii_case(flag) {
            return it.next();
        }
    }
    None
}

/// Exe name to look for before launching (lower case). An explicit name wins;
/// Squirrel apps (Discord, Slack) start via `Update.exe --processStart Real.exe`,
/// so the real name is used there. None for a shortcut, URI or non-exe.
pub fn process_name(command: &str, explicit: Option<&str>) -> Option<String> {
    if let Some(p) = explicit.map(str::trim).filter(|p| !p.is_empty()) {
        return Some(p.to_lowercase());
    }
    let (file, rest) = split_first(&expand_env(command));
    if let Some(real) = arg_after(&rest, "--processStart") {
        return Some(real.trim_matches('"').to_lowercase());
    }
    let f = crate::osutil::file_name(&file).to_lowercase();
    f.ends_with(".exe").then_some(f)
}

/// File to take an icon from (Squirrel: the real exe in the newest `app-x.y.z`).
pub fn icon_source(command: &str) -> Option<String> {
    let cmd = expand_env(command.trim());
    if cmd.to_lowercase().starts_with("shell:") {
        return None;
    }
    let (file, rest) = split_first(&cmd);
    let path = Path::new(&file);
    if path.file_name().is_some_and(|n| n.eq_ignore_ascii_case("update.exe")) {
        if let Some(target) = arg_after(&rest, "--processStart").map(|t| t.trim_matches('"').to_string()) {
            if !target.contains(['\\', '/']) {
                let dir = path.parent()?;
                let mut versions: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("app-")).map(|e| e.path().join(&target)).filter(|p| p.exists()).collect();
                versions.sort();
                if let Some(p) = versions.pop() {
                    return Some(p.to_string_lossy().into_owned());
                }
            }
        }
    }
    path.exists().then_some(file)
}

/// A command ready to start.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    pub file: String,
    pub args: String,
    pub cwd: Option<String>,
    /// opened by the shell (shortcut, URL, `shell:` URI, bare program name)
    pub shell: bool,
}


fn absolute(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with(r"\\") || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

/// Expand %VARS%, split, and decide how it must be opened. Refuses what cannot
/// be started safely instead of guessing.
pub fn prepare(command: &str) -> Result<Prepared, String> {
    let cmd = expand_env(command.trim());
    if cmd.is_empty() {
        return Err("empty command".into());
    }
    if cmd.chars().any(|c| c.is_control()) {
        return Err("the command contains control characters".into());
    }
    unambiguous(&cmd)?;
    let (file, args) = split_first(&cmd);
    if file.is_empty() {
        return Err("no program in the command".into());
    }
    if file.contains('"') {
        return Err("the program path contains a quote".into());
    }
    let low = file.to_lowercase();
    let uri = low.starts_with("shell:");
    // .bat/.cmd are not programs CreateProcess can start with a token of our choosing: the shell opens them
    // (ShellExecute "open", the arguments stay a separate string; no `cmd /c` line is ever built)
    let item = low.ends_with(".lnk") || low.ends_with(".url") || low.ends_with(".bat") || low.ends_with(".cmd") || uri;
    // a bare name ("notepad.exe") is resolved by the shell (App Paths, PATH), never by CreateProcess's search order
    let bare = !uri && !file.contains(['\\', '/', ':']);
    if !uri && !bare && !absolute(&file) {
        return Err("use a full path (relative paths are not started)".into());
    }
    let shell = item || bare;
    let cwd = if shell { None } else { Path::new(&file).parent().filter(|p| p.is_dir()).map(|p| p.to_string_lossy().into_owned()) };
    Ok(Prepared { file, args, cwd, shell })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// the user's normal interactive token
    User,
    /// explicit UAC prompt
    Uac,
    Refuse(&'static str),
}

/// `hayate_elevated` None = the token could not be read: fail closed.
pub fn route(admin: bool, hayate_elevated: Option<bool>) -> Route {
    match (admin, hayate_elevated) {
        (_, None) => Route::Refuse("Hayate could not tell whether it is running as administrator, so it started nothing."),
        (false, Some(_)) => Route::User,
        (true, Some(false)) => Route::Uac,
        (true, Some(true)) => Route::Refuse("Hayate is running as administrator: it does not start Admin entries from an elevated process. Start Hayate normally to launch it."),
    }
}

fn shell_exec(verb: &str, file: &str, args: &str, cwd: Option<&str>) -> Result<(), String> {
    let (v, f, a, d) = (wide(verb), wide(file), wide(args), wide(cwd.unwrap_or("")));
    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: pcw(&v),
        lpFile: pcw(&f),
        lpParameters: pcw(&a),
        lpDirectory: if cwd.is_some() { pcw(&d) } else { windows::core::PCWSTR::null() },
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE).is_ok();
        let r = ShellExecuteExW(&mut sei).map_err(|e| if e.code() == ERROR_CANCELLED.to_hresult() { "the UAC prompt was declined".to_string() } else { e.message() });
        if init {
            CoUninitialize();
        }
        r
    }
}

/// Open a shortcut / URL / `shell:` item or bare program name with the shell.
/// Only called from a Hayate that is not elevated (see `launch_as_user`).
pub fn shell_open(file: &str, args: &str, cwd: Option<&str>) -> Result<(), String> {
    shell_exec("open", file, args, cwd)
}

/// Start `command` the way its Admin flag says. Returns how it was started.
pub fn start(command: &str, admin: bool) -> Result<String, String> {
    let p = prepare(command)?;
    match route(admin, crate::osutil::elevation()) {
        Route::Refuse(why) => Err(why.into()),
        // Windows policy decides whether this prompts (it can be silent): do not say it did
        Route::Uac => shell_exec("runas", &p.file, &p.args, p.cwd.as_deref()).map(|_| "as administrator (runas)".into()),
        Route::User => crate::rules::actions::launch_as_user(&p.file, &p.args, p.cwd.as_deref(), p.shell),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_matches_launchpad() {
        assert_eq!(split_first(r#""C:\a b\x.exe" --y"#), (r"C:\a b\x.exe".into(), "--y".into()));
        assert_eq!(split_first(r"C:\Program Files\x\a.exe --min"), (r"C:\Program Files\x\a.exe".into(), "--min".into()));
        assert_eq!(split_first(r"C:\x\a.exe"), (r"C:\x\a.exe".into(), "".into()));
        assert_eq!(split_first("shell:AppsFolder\\X!App"), ("shell:AppsFolder\\X!App".into(), "".into()));
        let lnk = r"C:\Users\u\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\ShareX.lnk";
        assert_eq!(split_first(lnk), (lnk.to_string(), "".into()));
        assert_eq!(split_first(&format!("{lnk} --flag")), (lnk.to_string(), "--flag".into()));
        assert_eq!(split_first(r"C:\a b\run.cmd /q"), (r"C:\a b\run.cmd".into(), "/q".into()));
    }

    #[test]
    fn process_name_autodetects_squirrel_and_honours_explicit() {
        let discord = r#""C:\Users\u\AppData\Local\Discord\Update.exe" --processStart Discord.exe --process-start-args --start-minimized"#;
        assert_eq!(process_name(discord, None).as_deref(), Some("discord.exe"));
        assert_eq!(process_name(discord, Some("  Other.EXE ")).as_deref(), Some("other.exe"));
        assert_eq!(process_name(r"C:\x\Tool.exe -a", None).as_deref(), Some("tool.exe"));
        assert_eq!(process_name(r"C:\x\Start Thing.lnk", None), None);
        assert_eq!(process_name(r"shell:AppsFolder\X!App", None), None);
        assert_eq!(process_name(r"C:\x\a.exe", Some("   ")).as_deref(), Some("a.exe"), "a blank override falls back to autodetect");
    }

    #[test]
    fn prepare_decides_how_things_open() {
        let p = prepare(r#""C:\Tools\a b\x.exe" --k"#).unwrap();
        assert_eq!((p.file.as_str(), p.args.as_str(), p.shell), (r"C:\Tools\a b\x.exe", "--k", false));
        assert!(prepare(r"C:\x\y.lnk").unwrap().shell);
        let u = prepare(r"shell:AppsFolder\Microsoft.WindowsCalculator_8wekyb3d8bbwe!App").unwrap();
        assert!(u.shell && u.cwd.is_none());
        assert!(prepare("notepad.exe").unwrap().shell, "a bare name goes through the shell's resolution, not CreateProcess's search order");
        std::env::set_var("HAYATE_FX_DIR", r"C:\Fx Dir");
        assert_eq!(prepare(r"%HAYATE_FX_DIR%\a.exe /s").unwrap().file, r"C:\Fx Dir\a.exe");
    }

    #[test]
    fn prepare_refuses_what_cannot_be_started_safely() {
        for bad in ["", "   ", r"..\evil.exe", r"tools\a.exe", "C:a.exe", "\u{7}x.exe"] {
            assert!(prepare(bad).is_err(), "{bad:?}");
        }
        assert!(prepare("C:\\a\\\"b.exe x").is_err(), "a quote inside the program path");
    }

    #[test]
    fn admin_needs_a_prompt_and_never_from_an_elevated_hayate() {
        assert_eq!(route(false, Some(false)), Route::User);
        assert_eq!(route(false, Some(true)), Route::User, "non-admin from an elevated Hayate is still the normal user token");
        assert_eq!(route(true, Some(false)), Route::Uac);
        assert!(matches!(route(true, Some(true)), Route::Refuse(w) if w.contains("elevated")));
    }

    #[test]
    fn an_unknown_elevation_state_starts_nothing() {
        assert!(matches!(route(false, None), Route::Refuse(_)), "even a normal entry: the token could not be verified");
        assert!(matches!(route(true, None), Route::Refuse(_)));
    }

    #[test]
    fn batch_files_are_opened_by_the_shell_with_separate_arguments() {
        let p = prepare(r#""C:\Tools\run it.bat" /q "a b""#).unwrap();
        assert!(p.shell, ".bat goes through ShellExecute open, not CreateProcess");
        assert_eq!((p.file.as_str(), p.args.as_str()), (r"C:\Tools\run it.bat", r#"/q "a b""#), "args stay a separate string; no cmd /c line");
        assert!(prepare(r"C:\Tools\x.cmd").unwrap().shell);
    }

    #[test]
    fn an_unquoted_path_that_could_end_in_two_places_is_refused_not_guessed() {
        // `a.exe` could be the program (args `b.exe --x`) or the start of a file called `a.exe b.exe`
        let e = prepare(r"C:\Nope Dir\a.exe b.exe --x").unwrap_err();
        assert!(e.contains("ambiguous") && e.contains("quotes"), "{e}");
        // quoting removes the ambiguity
        assert!(prepare(r#""C:\Nope Dir\a.exe b.exe" --x"#).is_ok());
        // a single reading is fine
        assert!(prepare(r"C:\Program Files\x\a.exe --min").is_ok());
    }

    #[test]
    fn an_existing_file_anchors_an_otherwise_ambiguous_path() {
        let d = std::env::temp_dir().join(format!("HayateStartupFixture-launch-{}", std::process::id()));
        let dir = d.join("a.exe b"); // a FOLDER whose name starts like a program
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real.exe");
        std::fs::write(&real, b"x").unwrap();
        let cmd = format!("{} --x", real.display());
        let (f, a) = split_first(&cmd);
        assert_eq!((f.as_str(), a.as_str()), (real.to_str().unwrap(), "--x"), "the folder is not the program");
        assert!(unambiguous(&cmd).is_ok(), "exactly one reading is a file on disk");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn start_refuses_before_touching_anything_on_a_bad_command() {
        assert!(start("", false).is_err());
        assert!(start(r"..\x.exe", true).is_err());
    }
}
