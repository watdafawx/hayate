//! Pure parts of the self-updater: config + state files, version labels,
//! the stage / apply / rollback decisions and the relaunch command line.
//! No I/O here, so all of it is unit-tested.

/// `%LOCALAPPDATA%\Blink\update.ini`, written by install.ps1 (user-owned).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    /// Folder with blink.exe + blink.exe.sha256 + version.txt (the publish step output).
    pub source: Option<String>,
    /// Folder of the installed blink.exe. Only a copy running from here updates itself.
    pub install_dir: Option<String>,
    pub enabled: bool,
}

pub fn parse_config(text: &str) -> Config {
    let mut c = Config { enabled: true, ..Default::default() };
    for line in text.lines() {
        let line = line.split(';').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().trim_matches('"').trim();
        let v = if v.is_empty() { None } else { Some(v.to_string()) };
        match k.trim().to_ascii_lowercase().as_str() {
            "source" => c.source = v,
            "install_dir" => c.install_dir = v,
            "enabled" => c.enabled = !matches!(v.as_deref().map(str::to_ascii_lowercase).as_deref(), Some("false" | "0" | "no" | "off")),
            _ => {}
        }
    }
    c
}

/// Case-insensitive path compare, ignoring a trailing slash and `/` vs `\`.
pub fn same_dir(a: &str, b: &str) -> bool {
    let n = |s: &str| s.trim().trim_end_matches(['\\', '/']).replace('/', "\\").to_ascii_lowercase();
    !a.trim().is_empty() && n(a) == n(b)
}

/// Lexical normal form of a Windows path for comparisons: `\` separators,
/// lower case, no `\\?\` prefix, no trailing `\`, `.` and `..` folded.
pub fn norm_path(p: &str) -> String {
    let s = p.trim().replace('/', "\\");
    let s = if let Some(r) = s.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{r}")
    } else if let Some(r) = s.strip_prefix("\\\\?\\") {
        r.to_string()
    } else {
        s
    };
    let unc = s.starts_with("\\\\");
    let mut parts: Vec<&str> = Vec::new();
    for seg in s.split('\\') {
        match seg {
            "" | "." => {}
            ".." => {
                // never fold away the drive / UNC server+share root
                if parts.len() > if unc { 2 } else { 1 } {
                    parts.pop();
                }
            }
            x => parts.push(x),
        }
    }
    let j = parts.join("\\").to_ascii_lowercase();
    if unc {
        format!("\\\\{j}")
    } else {
        j
    }
}

/// `child` is `parent` or anywhere below it (lexical; see `norm_path`).
pub fn inside_or_same(child: &str, parent: &str) -> bool {
    let (c, p) = (norm_path(child), norm_path(parent));
    !p.is_empty() && (c == p || c.strip_prefix(&p).is_some_and(|r| r.starts_with('\\')))
}

/// The update source must be a folder outside the install dir: a drop folder
/// inside it would let whatever writes there feed the swap from within.
pub fn source_allowed(source: &str, install: &str) -> Result<(), &'static str> {
    if source.trim().is_empty() {
        Err("no source")
    } else if same_dir(source, install) || inside_or_same(source, install) {
        Err("source is the install dir or inside it")
    } else {
        Ok(())
    }
}

/// `blink.exe.sha256`: first token, 64 hex digits (lowercased). Accepts
/// `<hex>`, `<hex>  blink.exe` and PowerShell's upper-case output.
pub fn parse_sha_file(text: &str) -> Option<String> {
    let t = text.trim_start_matches('\u{feff}').split_whitespace().next()?;
    (t.len() == 64 && t.bytes().all(|b| b.is_ascii_hexdigit())).then(|| t.to_ascii_lowercase())
}

/// `version.txt` from the publish step: `key=value` lines (sha, commit, built).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct VersionInfo {
    pub sha: String,
    pub commit: String,
    pub built: String,
}

pub fn parse_version(text: &str) -> VersionInfo {
    let mut v = VersionInfo::default();
    for line in text.lines() {
        if let Some((k, val)) = line.trim_start_matches('\u{feff}').split_once('=') {
            let val = val.trim().to_string();
            match k.trim() {
                "sha" => v.sha = val,
                "commit" => v.commit = val,
                "built" => v.built = val,
                _ => {}
            }
        }
    }
    v
}

/// Short label for logs / status bar: git sha from version.txt, else the
/// first 7 hex digits of the exe hash (prefixed `#`).
pub fn label(version: Option<&VersionInfo>, exe_sha: &str) -> String {
    match version.filter(|v| !v.sha.is_empty()) {
        Some(v) => v.sha.clone(),
        None if exe_sha.len() >= 7 => format!("#{}", &exe_sha[..7]),
        None => "?".into(),
    }
}

// ---- state file (app-owned): %LOCALAPPDATA%\Blink\update-state.ini --------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// exe sha256 being tried right now (set by the old instance before relaunch)
    pub trial: String,
    /// exe sha256 it replaced (blink.old.exe)
    pub from: String,
    /// pid of the trial instance once it started (written by the trial itself)
    pub trial_pid: u32,
    /// pid of the trial instance that was closed by the user (X, Esc, tray Quit) before it
    /// was ready: a clean quit, not a failed start (written by the trial itself)
    pub quit_pid: u32,
    /// = trial once the trial instance ran ~10 s with its window up
    pub ready: String,
    /// builds that failed their trial (never staged again), newest last, max 8
    pub bad: Vec<String>,
    /// last successful swap: label + unix time (for the "Updated to" note)
    pub updated_to: String,
    pub updated_at: u64,
    /// last rollback: label of the failed build + unix time
    pub rolled_back: String,
    pub rolled_back_at: u64,
    /// last UAC-gated swap the user cancelled (or that could not ask): label + unix time
    pub declined: String,
    pub declined_at: u64,
}

pub fn parse_state(text: &str) -> State {
    let mut s = State::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "trial" => s.trial = v.into(),
            "from" => s.from = v.into(),
            "trial_pid" => s.trial_pid = v.parse().unwrap_or(0),
            "quit_pid" => s.quit_pid = v.parse().unwrap_or(0),
            "ready" => s.ready = v.into(),
            "bad" => s.bad = v.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect(),
            "updated_to" => s.updated_to = v.into(),
            "updated_at" => s.updated_at = v.parse().unwrap_or(0),
            "rolled_back" => s.rolled_back = v.into(),
            "rolled_back_at" => s.rolled_back_at = v.parse().unwrap_or(0),
            "declined" => s.declined = v.into(),
            "declined_at" => s.declined_at = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    s
}

pub fn write_state(s: &State) -> String {
    format!(
        "; Hayate self-update state (written by Hayate; safe to delete)\ntrial={}\nfrom={}\ntrial_pid={}\nquit_pid={}\nready={}\nbad={}\nupdated_to={}\nupdated_at={}\nrolled_back={}\nrolled_back_at={}\ndeclined={}\ndeclined_at={}\n",
        s.trial,
        s.from,
        s.trial_pid,
        s.quit_pid,
        s.ready,
        s.bad.join(","),
        s.updated_to,
        s.updated_at,
        s.rolled_back,
        s.rolled_back_at,
        s.declined,
        s.declined_at
    )
}

impl State {
    pub fn mark_bad(&mut self, sha: &str) {
        if sha.is_empty() || self.bad.iter().any(|b| b == sha) {
            return;
        }
        self.bad.push(sha.to_string());
        let n = self.bad.len();
        if n > 8 {
            self.bad.drain(..n - 8);
        }
    }
}

// ---- decisions ---------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub enum Stage {
    /// nothing new (same build, a known-bad build, or already staged)
    Nothing(&'static str),
    /// copy + verify this build into blink.new.exe
    Stage,
}

/// Whether the build in the source (its sha256) should be staged.
pub fn should_stage(source_sha: &str, current_sha: &str, staged_sha: Option<&str>, bad: &[String]) -> Stage {
    if source_sha == current_sha {
        Stage::Nothing("same build")
    } else if bad.iter().any(|b| b == source_sha) {
        Stage::Nothing("known bad build")
    } else if staged_sha == Some(source_sha) {
        Stage::Nothing("already staged")
    } else {
        Stage::Stage
    }
}

pub const IDLE_MS: u32 = 120_000;

/// A staged build is swapped in when the window is hidden (resident in the
/// tray) or nobody has touched mouse/keyboard for ~2 min.
pub fn good_moment(window_visible: bool, idle_ms: u32) -> bool {
    !window_visible || idle_ms >= IDLE_MS
}

/// How a staged build goes live.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Apply {
    /// not elevated: swap + relaunch by itself at a good moment, same token
    /// (the new build gets nothing the user's own processes do not have)
    Auto,
    /// elevated: the source folder is writable by unelevated processes, so a
    /// new build never inherits this token. It goes live only when the user
    /// clicks "restart", and starts through a UAC prompt for that exe.
    Ask,
}

pub fn apply_mode(elevated: bool) -> Apply {
    if elevated {
        Apply::Ask
    } else {
        Apply::Auto
    }
}

/// Swap the staged build in now? `clicked`: the user pressed "restart".
pub fn swap_now(mode: Apply, clicked: bool, window_visible: bool, idle_ms: u32) -> bool {
    match mode {
        Apply::Auto => clicked || good_moment(window_visible, idle_ms),
        // never at an idle / hidden "good moment"
        Apply::Ask => clicked,
    }
}

/// Status-bar offer while an elevated Blink has a verified build staged.
pub fn offer_text(label: &str, declined: bool) -> String {
    if declined {
        format!("UAC cancelled · update {label} still ready · restart")
    } else {
        format!("Update ready · {label} · restart")
    }
}

// ---- manual "Check for updates" ---------------------------------------------------

/// What a user-clicked check decides. Only a manual check may override the
/// bad / declined marks, and only for the build the source holds right now.
#[derive(Debug, PartialEq)]
pub enum Manual {
    /// the source holds the running build: nothing to do, state untouched
    UpToDate,
    /// that build is already staged and verified: nothing to copy
    AlreadyStaged,
    /// copy + verify it. `retry`: it had been marked bad (rolled back earlier)
    /// and that mark is cleared now (`st` was changed: save it).
    Stage { retry: bool },
}

/// Decide a manual check for the build in the source (`source_sha`, shown as
/// `source_label`). Clears `bad` for that exact hash, and `rolled_back` /
/// `declined` when they name that same label. Other hashes, the trial fields
/// and the update note are never touched.
pub fn manual_check(source_sha: &str, current_sha: &str, staged_sha: Option<&str>, source_label: &str, st: &mut State) -> Manual {
    if source_sha == current_sha {
        return Manual::UpToDate;
    }
    if staged_sha == Some(source_sha) {
        return Manual::AlreadyStaged;
    }
    let was_bad = st.bad.iter().any(|b| b == source_sha);
    st.bad.retain(|b| b != source_sha);
    if was_bad && st.rolled_back == source_label {
        st.rolled_back.clear();
        st.rolled_back_at = 0;
    }
    if st.declined == source_label {
        st.declined.clear();
        st.declined_at = 0;
    }
    Manual::Stage { retry: was_bad }
}

/// Status line / Settings text for a finished manual check.
pub fn manual_text(r: &Manual, label: &str) -> String {
    match r {
        Manual::UpToDate => format!("Up to date ({label})"),
        Manual::AlreadyStaged | Manual::Stage { retry: false } => format!("Update {label} available \u{2014} Restart to apply"),
        Manual::Stage { retry: true } => format!("Update {label} was rolled back earlier; trying again"),
    }
}

pub fn manual_error(why: &str) -> String {
    format!("Update check failed: {why}")
}

/// Status-bar note after a rollback: click it to run a manual check.
pub fn rolled_back_note(label: &str) -> String {
    format!("Update {label} failed at start; rolled back \u{b7} click to try again")
}

// ---- UAC relaunch (elevated Blink only) ------------------------------------------
//
// `runas` from an already elevated process starts the child elevated with no
// prompt, so consent has to be asked from an unelevated process: the elevated
// old instance has Explorer start `blink.old.exe --update-elevate …` (this
// build, unelevated), which ShellExecuteExW("runas")s the new blink.exe and
// writes the outcome to a result file. The old instance waits for that file,
// then watches the new pid as before.

pub const HELPER_ARG: &str = "--update-elevate";

/// `--update-elevate <nonce> <result file> <exe> [args…]`
#[derive(Clone, Debug, PartialEq)]
pub struct Helper {
    pub nonce: String,
    pub result: String,
    pub exe: String,
    pub args: Vec<String>,
}

pub fn helper_args(args: &[String]) -> Option<Helper> {
    if args.first().map(String::as_str) != Some(HELPER_ARG) || args.len() < 4 {
        return None;
    }
    let (nonce, result, exe) = (&args[1], &args[2], &args[3]);
    if nonce.is_empty() || result.is_empty() || exe.is_empty() {
        return None;
    }
    Some(Helper { nonce: nonce.clone(), result: result.clone(), exe: exe.clone(), args: args[4..].to_vec() })
}

pub fn helper_cmd(helper_exe: &str, h: &Helper) -> String {
    let mut a = vec![HELPER_ARG.to_string(), h.nonce.clone(), h.result.clone(), h.exe.clone()];
    a.extend(h.args.iter().cloned());
    command_line(helper_exe, &a)
}

/// What the helper reports.
#[derive(Clone, Debug, PartialEq)]
pub enum Uac {
    /// no (matching) answer yet
    Pending,
    /// consent given: the elevated new build runs as this pid
    Launched(u32),
    /// the user said No (ERROR_CANCELLED)
    Cancelled,
    /// ShellExecuteExW failed with this Win32 error
    Failed(u32),
}

pub fn write_uac(nonce: &str, r: &Uac) -> String {
    match r {
        Uac::Pending => format!("{nonce} pending\n"),
        Uac::Launched(pid) => format!("{nonce} launched {pid}\n"),
        Uac::Cancelled => format!("{nonce} cancelled\n"),
        Uac::Failed(e) => format!("{nonce} failed {e}\n"),
    }
}

/// A result for another nonce (a stale file) reads as Pending.
pub fn parse_uac(text: &str, nonce: &str) -> Uac {
    let mut it = text.split_whitespace();
    if nonce.is_empty() || it.next() != Some(nonce) {
        return Uac::Pending;
    }
    let num = |s: Option<&str>| s.and_then(|v| v.parse::<u32>().ok());
    match it.next() {
        Some("launched") => match num(it.next()) {
            Some(p) if p != 0 => Uac::Launched(p),
            _ => Uac::Failed(0),
        },
        Some("cancelled") => Uac::Cancelled,
        Some("failed") => Uac::Failed(num(it.next()).unwrap_or(0)),
        _ => Uac::Pending,
    }
}

/// How long the old instance waits for an answer to the UAC prompt.
pub const UAC_LIMIT_MS: u64 = 300_000;

#[derive(Debug, PartialEq)]
pub enum UacStep {
    Wait,
    /// watch this pid's trial start (watchdog / rollback as usual)
    Watch(u32),
    /// put the current build back and keep running it; the new one is not bad
    Undo(&'static str),
}

pub fn uac_step(r: &Uac, elapsed_ms: u64) -> UacStep {
    match r {
        Uac::Launched(pid) => UacStep::Watch(*pid),
        Uac::Cancelled => UacStep::Undo("UAC prompt cancelled"),
        Uac::Failed(_) => UacStep::Undo("elevated start failed"),
        Uac::Pending if elapsed_ms >= UAC_LIMIT_MS => UacStep::Undo("no answer to the UAC prompt"),
        Uac::Pending => UacStep::Wait,
    }
}

#[derive(Debug, PartialEq)]
pub enum Trial {
    Wait,
    /// the trial instance reached "ready"
    Ok,
    /// the user closed it on purpose (X, Esc, tray Quit) before ready: exit code 0 and the
    /// instance said so in the state file. Not a failed build: keep it, mark nothing bad.
    UserQuit,
    /// it crashed, exited without saying why, or hung before ready: restore blink.old.exe
    Rollback(&'static str),
}

/// How long the trial instance gets to reach "ready" (it marks itself after ~10 s).
pub const TRIAL_LIMIT_MS: u64 = 40_000;

/// The old instance (now only a launcher/watchdog) decides on the trial.
pub fn trial_outcome(child_exited: bool, state: &State, trial_sha: &str, child_pid: u32, elapsed_ms: u64) -> Trial {
    let ready = state.trial == trial_sha && state.ready == trial_sha && state.trial_pid == child_pid;
    if ready {
        Trial::Ok
    } else if child_exited && state.trial == trial_sha && state.quit_pid == child_pid && child_pid != 0 {
        Trial::UserQuit
    } else if child_exited {
        Trial::Rollback("exited before ready")
    } else if elapsed_ms >= TRIAL_LIMIT_MS {
        Trial::Rollback("no ready signal in time")
    } else {
        Trial::Wait
    }
}

// ---- relaunch: window state + command line -------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Show {
    /// resident in the tray, window hidden
    #[default]
    Hidden,
    /// shown and in front: show + activate
    Front,
    /// shown behind other windows: show without activating, at the bottom
    Back,
    /// minimized
    Min,
}

impl Show {
    fn key(self) -> &'static str {
        match self {
            Show::Hidden => "hidden",
            Show::Front => "front",
            Show::Back => "back",
            Show::Min => "min",
        }
    }
    fn from_key(s: &str) -> Option<Show> {
        Some(match s {
            "hidden" => Show::Hidden,
            "front" => Show::Front,
            "back" => Show::Back,
            "min" => Show::Min,
            _ => return None,
        })
    }
}

/// What the relaunched instance restores. Passed as private `--update-*` args.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Restore {
    /// exe sha256 of the build under trial (None after a rollback relaunch)
    pub trial: Option<String>,
    pub show: Option<Show>,
    pub tab: Option<usize>,
    /// normal (restored) window rect: left, top, right, bottom
    pub place: Option<(i32, i32, i32, i32)>,
    pub maximized: bool,
    /// BLINK_DATA_DIR / BLINK_MULTI of the old instance. Only set on the UAC
    /// path: that relaunch goes through Explorer and loses our environment.
    pub data_dir: Option<String>,
    pub multi: bool,
}

// `--tab <name>`, `--open`, `--restart-admin` (taskbar.rs) are one-shot: a relaunch must never replay them
const PRIVATE: [&str; 8] = ["--update-trial", "--update-show", "--update-tab", "--update-place", "--update-max", "--wait-pid", "--update-data-dir", "--tab"];
// `--startup-logon` is the startup manager's private proof of "started by Blink's own autorun": an update or restart must never carry it over
const PRIVATE_FLAGS: [&str; 6] = ["--update-max", "--update-multi", "--open", "--restart-admin", "--replace-for-admin", "--startup-logon"];

/// Args minus the private relaunch ones (and their values).
pub fn user_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if PRIVATE_FLAGS.contains(&a.as_str()) {
            i += 1;
        } else if PRIVATE.contains(&a.as_str()) {
            i += 2;
        } else {
            out.push(a.clone());
            i += 1;
        }
    }
    out
}

pub fn parse_restore(args: &[String]) -> Restore {
    let mut r = Restore::default();
    let val = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).map(String::as_str);
    r.trial = val("--update-trial").filter(|s| s.len() == 64).map(String::from);
    r.show = val("--update-show").and_then(Show::from_key);
    r.tab = val("--update-tab").and_then(|v| v.parse().ok());
    r.place = val("--update-place").and_then(|v| {
        let n: Vec<i32> = v.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        (n.len() == 4 && n[2] > n[0] && n[3] > n[1]).then(|| (n[0], n[1], n[2], n[3]))
    });
    r.maximized = args.iter().any(|a| a == "--update-max");
    r.data_dir = val("--update-data-dir").filter(|v| !v.trim().is_empty()).map(String::from);
    r.multi = args.iter().any(|a| a == "--update-multi");
    r
}

/// `[user args] + private restore args`, each quoted as needed.
pub fn relaunch_args(args: &[String], r: &Restore) -> Vec<String> {
    let mut v = user_args(args);
    if let Some(t) = &r.trial {
        v.extend(["--update-trial".into(), t.clone()]);
    }
    if let Some(s) = r.show {
        v.extend(["--update-show".into(), s.key().into()]);
    }
    if let Some(t) = r.tab {
        v.extend(["--update-tab".into(), t.to_string()]);
    }
    if let Some((l, t, rr, b)) = r.place {
        v.extend(["--update-place".into(), format!("{l},{t},{rr},{b}")]);
    }
    if r.maximized {
        v.push("--update-max".into());
    }
    if let Some(d) = &r.data_dir {
        v.extend(["--update-data-dir".into(), d.clone()]);
    }
    if r.multi {
        v.push("--update-multi".into());
    }
    v
}

/// Args of the elevated copy started by "Restart as admin": the private restore
/// args (test environment, tab) plus `--wait-pid` so it waits for this process to
/// exit. Nothing here is a user-supplied command: only the fixed private flags and
/// their values, each quoted by `join_args`.
pub fn admin_relaunch_args(r: &Restore, wait_pid: u32) -> Vec<String> {
    let mut v = relaunch_args(&[], r);
    v.extend(["--wait-pid".into(), wait_pid.to_string()]);
    v
}

/// Full command line for CreateProcessW: quoted exe + args (MSVC rules).
pub fn command_line(exe: &str, args: &[String]) -> String {
    let a = join_args(args);
    if a.is_empty() {
        format!("\"{exe}\"")
    } else {
        format!("\"{exe}\" {a}")
    }
}

/// Args only (ShellExecuteExW lpParameters), each quoted as needed.
pub fn join_args(args: &[String]) -> String {
    args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
}

fn quote(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_string();
    }
    let mut out = String::from("\"");
    let mut bs = 0;
    for c in a.chars() {
        match c {
            '\\' => bs += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', bs * 2 + 1));
                out.push('"');
                bs = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', bs));
                out.push(c);
                bs = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', bs * 2));
    out.push('"');
    out
}

/// "3 min ago" style for the status bar.
pub fn ago(secs: u64) -> String {
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        _ => format!("{} h ago", secs / 3600),
    }
}

/// How long the "Updated to …" / "Update … failed" note stays up.
pub const NOTE_SECS: u64 = 3600;

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn config() {
        let c = parse_config("; c\n[update]\nsource = X:\\Dev\\blink\\dist\ninstall_dir = \"C:\\U\\Programs\\Blink\" ; note\n");
        assert_eq!(c.source.as_deref(), Some("X:\\Dev\\blink\\dist"));
        assert_eq!(c.install_dir.as_deref(), Some("C:\\U\\Programs\\Blink"));
        assert!(c.enabled);
        let c = parse_config("source =\nenabled = false\n");
        assert_eq!(c.source, None);
        assert!(!c.enabled);
        assert_eq!(parse_config(""), Config { enabled: true, ..Default::default() });
    }

    #[test]
    fn dirs() {
        assert!(same_dir("C:\\Users\\M\\AppData\\Local\\Programs\\Blink\\", "c:/users/m/appdata/local/programs/blink"));
        assert!(!same_dir("C:\\Dev\\blink\\target\\release", "C:\\Users\\M\\AppData\\Local\\Programs\\Blink"));
        assert!(!same_dir("", ""));
    }

    #[test]
    fn sha_file() {
        assert_eq!(parse_sha_file(&format!("{}  blink.exe\r\n", A.to_uppercase())).as_deref(), Some(A));
        assert_eq!(parse_sha_file(&format!("\u{feff}{B}")).as_deref(), Some(B));
        assert_eq!(parse_sha_file("abc"), None);
        assert_eq!(parse_sha_file(""), None);
        assert_eq!(parse_sha_file(&"g".repeat(64)), None);
    }

    #[test]
    fn version_label() {
        let v = parse_version("sha=1a2b3c4\ncommit=2026-11-16 10:00\nbuilt=2026-11-16 10:05\n");
        assert_eq!(v.sha, "1a2b3c4");
        assert_eq!(v.built, "2026-11-16 10:05");
        assert_eq!(label(Some(&v), A), "1a2b3c4");
        assert_eq!(label(None, A), "#aaaaaaa");
        assert_eq!(label(Some(&VersionInfo::default()), B), "#bbbbbbb");
    }

    #[test]
    fn state_roundtrip() {
        let mut st = State { trial: A.into(), from: B.into(), trial_pid: 42, ready: A.into(), updated_to: "1a2b3c4".into(), updated_at: 99, declined: "9f8e7d6".into(), declined_at: 77, ..Default::default() };
        st.mark_bad("x");
        st.mark_bad("x");
        st.mark_bad("y");
        let back = parse_state(&write_state(&st));
        assert_eq!(back, st);
        assert_eq!(back.bad, vec!["x", "y"]);
        for i in 0..20 {
            st.mark_bad(&format!("b{i}"));
        }
        assert_eq!(st.bad.len(), 8);
        assert_eq!(st.bad.last().unwrap(), "b19");
        assert_eq!(parse_state("garbage\n"), State::default());
    }

    #[test]
    fn stage_decision() {
        assert_eq!(should_stage(A, A, None, &[]), Stage::Nothing("same build"));
        assert_eq!(should_stage(B, A, None, &[]), Stage::Stage);
        assert_eq!(should_stage(B, A, Some(B), &[]), Stage::Nothing("already staged"));
        assert_eq!(should_stage(B, A, Some("old"), &[]), Stage::Stage);
        assert_eq!(should_stage(B, A, None, &[B.to_string()]), Stage::Nothing("known bad build"));
        // a published older build (master rolled back) is staged too: the source is the authority
        assert_eq!(should_stage(B, A, None, &[A.to_string()]), Stage::Stage);
    }

    #[test]
    fn moment() {
        assert!(good_moment(false, 0));
        assert!(!good_moment(true, 5_000));
        assert!(!good_moment(true, IDLE_MS - 1));
        assert!(good_moment(true, IDLE_MS));
    }

    #[test]
    fn trial() {
        let mut st = State { trial: A.into(), trial_pid: 7, ..Default::default() };
        assert_eq!(trial_outcome(false, &st, A, 7, 1000), Trial::Wait);
        assert_eq!(trial_outcome(true, &st, A, 7, 1000), Trial::Rollback("exited before ready"));
        assert_eq!(trial_outcome(false, &st, A, 7, TRIAL_LIMIT_MS), Trial::Rollback("no ready signal in time"));
        st.ready = A.into();
        assert_eq!(trial_outcome(false, &st, A, 7, 1000), Trial::Ok);
        // ready after it exited still counts (it crashed later: not a start crash)
        assert_eq!(trial_outcome(true, &st, A, 7, 20_000), Trial::Ok);
        // a stale "ready" from another pid or build does not count
        assert_eq!(trial_outcome(true, &st, A, 8, 1000), Trial::Rollback("exited before ready"));
        assert_eq!(trial_outcome(true, &st, B, 7, 1000), Trial::Rollback("exited before ready"));
    }

    #[test]
    fn a_user_quit_inside_the_trial_is_not_a_failed_build() {
        // the instance wrote "quit_pid" before it closed its window on the user's request
        let st = State { trial: A.into(), trial_pid: 7, quit_pid: 7, ..Default::default() };
        assert_eq!(trial_outcome(true, &st, A, 7, 1000), Trial::UserQuit);
        // still running: wait for the exit, never decide early
        assert_eq!(trial_outcome(false, &st, A, 7, 1000), Trial::Wait);
        // a crash (no marker), a marker of another pid or of another trial: still a failure
        let crash = State { trial: A.into(), trial_pid: 7, ..Default::default() };
        assert_eq!(trial_outcome(true, &crash, A, 7, 1000), Trial::Rollback("exited before ready"));
        let other_pid = State { trial: A.into(), trial_pid: 7, quit_pid: 6, ..Default::default() };
        assert_eq!(trial_outcome(true, &other_pid, A, 7, 1000), Trial::Rollback("exited before ready"));
        assert_eq!(trial_outcome(true, &st, B, 7, 1000), Trial::Rollback("exited before ready"));
        // pid 0 is "nobody"
        let zero = State { trial: A.into(), quit_pid: 0, ..Default::default() };
        assert_eq!(trial_outcome(true, &zero, A, 0, 1000), Trial::Rollback("exited before ready"));
        // ready wins over the marker
        let both = State { trial: A.into(), ready: A.into(), trial_pid: 7, quit_pid: 7, ..Default::default() };
        assert_eq!(trial_outcome(true, &both, A, 7, 1000), Trial::Ok);
    }

    #[test]
    fn the_quit_marker_survives_the_state_file() {
        let st = State { trial: A.into(), trial_pid: 7, quit_pid: 7, ..Default::default() };
        assert_eq!(parse_state(&write_state(&st)).quit_pid, 7);
        assert_eq!(parse_state("trial=x
").quit_pid, 0);
    }

    #[test]
    fn the_startup_logon_proof_never_survives_an_update_or_restart() {
        let orig = s(&["--resident", "--startup-logon", "--wait-pid", "123"]);
        assert_eq!(user_args(&orig), s(&["--resident"]), "the private flag is stripped like the other one-shot flags");
        let v = relaunch_args(&orig, &Restore::default());
        assert!(!v.iter().any(|a| a == "--startup-logon"), "{v:?}");
        assert_eq!(v[0], "--resident");
    }

    #[test]
    fn restore_args() {
        let orig = s(&["--resident", "--wait-pid", "123", "--update-trial", A, "--update-max"]);
        assert_eq!(user_args(&orig), s(&["--resident"]));
        let r = Restore { trial: Some(B.into()), show: Some(Show::Back), tab: Some(3), place: Some((10, 20, 1130, 740)), maximized: true, data_dir: Some(r"G:\t\data dir".into()), multi: true };
        let v = relaunch_args(&orig, &r);
        assert_eq!(v[0], "--resident");
        assert_eq!(parse_restore(&v), r);
        assert_eq!(user_args(&v), s(&["--resident"]));
        // garbage values are ignored
        let p = parse_restore(&s(&["--update-show", "sideways", "--update-place", "5,5,1,1", "--update-trial", "short"]));
        assert_eq!(p, Restore::default());
        assert_eq!(parse_restore(&s(&[])), Restore::default());
    }

    #[test]
    fn source_inside_install_rejected() {
        let inst = r"C:\Users\M\AppData\Local\Programs\Blink";
        assert!(source_allowed(r"X:\Dev\blink\dist", inst).is_ok());
        assert!(source_allowed(r"\\fox\dist", inst).is_ok());
        assert!(source_allowed(inst, inst).is_err());
        assert!(source_allowed(&format!(r"{inst}\"), inst).is_err());
        assert!(source_allowed(&format!(r"{inst}\drop"), inst).is_err());
        assert!(source_allowed(&format!(r"{}\drop\x", inst.to_uppercase()), inst).is_err());
        assert!(source_allowed("c:/users/m/appdata/local/programs/blink/sub", inst).is_err());
        assert!(source_allowed(&format!(r"{inst}\a\..\b"), inst).is_err());
        assert!(source_allowed(&format!(r"\\?\{inst}\drop"), inst).is_err());
        // .. that leaves the install dir is fine; a sibling with the same prefix is not "inside"
        assert!(source_allowed(&format!(r"{inst}\..\dist"), inst).is_ok());
        assert!(source_allowed(&format!("{inst}Dist"), inst).is_ok());
        assert!(source_allowed("  ", inst).is_err());
        assert_eq!(norm_path(r"C:\..\..\x"), r"c:\x");
        assert_eq!(norm_path(r"\\?\UNC\srv\share\..\..\a"), r"\\srv\share\a");
    }

    #[test]
    fn elevated_never_swaps_by_itself() {
        assert_eq!(apply_mode(false), Apply::Auto);
        assert_eq!(apply_mode(true), Apply::Ask);
        // not elevated: unchanged (hidden or 2 min idle)
        assert!(swap_now(Apply::Auto, false, false, 0));
        assert!(swap_now(Apply::Auto, false, true, IDLE_MS));
        assert!(!swap_now(Apply::Auto, false, true, 5_000));
        // elevated: hidden / idle are never enough, only the click
        assert!(!swap_now(Apply::Ask, false, false, 0));
        assert!(!swap_now(Apply::Ask, false, true, IDLE_MS * 10));
        assert!(!swap_now(Apply::Ask, false, false, u32::MAX));
        assert!(swap_now(Apply::Ask, true, true, 0));
        assert!(swap_now(Apply::Ask, true, false, 0));
        assert_eq!(offer_text("1a2b3c4", false), "Update ready · 1a2b3c4 · restart");
        assert!(offer_text("1a2b3c4", true).contains("UAC cancelled"));
    }

    #[test]
    fn uac_helper_protocol() {
        let h = Helper { nonce: "n1".into(), result: r"C:\D\update-uac.txt".into(), exe: r"C:\P B\blink.exe".into(), args: s(&["--update-trial", A, "--update-data-dir", r"G:\a b"]) };
        let line = helper_cmd(r"C:\P B\blink.old.exe", &h);
        assert!(line.starts_with(r#""C:\P B\blink.old.exe" --update-elevate n1 C:\D\update-uac.txt "C:\P B\blink.exe" --update-trial "#), "{line}");
        assert!(line.ends_with(r#"--update-data-dir "G:\a b""#), "{line}");
        // what the helper sees as argv[1..] after the C runtime splits that line
        let mut argv = vec![HELPER_ARG.to_string(), h.nonce.clone(), h.result.clone(), h.exe.clone()];
        argv.extend(h.args.iter().cloned());
        assert_eq!(helper_args(&argv), Some(h.clone()));
        assert_eq!(helper_args(&s(&["--update-elevate", "n1", "r"])), None);
        assert_eq!(helper_args(&s(&["--resident", "n1", "r", "x"])), None);
        assert_eq!(helper_args(&s(&["--update-elevate", "", "r", "x"])), None);
        for r in [Uac::Launched(4242), Uac::Cancelled, Uac::Failed(5)] {
            assert_eq!(parse_uac(&write_uac("n1", &r), "n1"), r);
        }
        // a stale answer (other nonce), an empty file, junk: still waiting
        assert_eq!(parse_uac(&write_uac("old", &Uac::Launched(7)), "n1"), Uac::Pending);
        assert_eq!(parse_uac("", "n1"), Uac::Pending);
        assert_eq!(parse_uac("n1 launched 7", ""), Uac::Pending);
        assert_eq!(parse_uac(&write_uac("n1", &Uac::Pending), "n1"), Uac::Pending);
        assert_eq!(parse_uac("n1 launched 0", "n1"), Uac::Failed(0));
        assert_eq!(parse_uac("n1 launched x", "n1"), Uac::Failed(0));
    }

    #[test]
    fn uac_decision() {
        assert_eq!(uac_step(&Uac::Pending, 1000), UacStep::Wait);
        assert_eq!(uac_step(&Uac::Pending, UAC_LIMIT_MS), UacStep::Undo("no answer to the UAC prompt"));
        assert_eq!(uac_step(&Uac::Launched(9), 1000), UacStep::Watch(9));
        // cancel / failure: keep the current build (no rollback, nothing marked bad)
        assert_eq!(uac_step(&Uac::Cancelled, 1000), UacStep::Undo("UAC prompt cancelled"));
        assert_eq!(uac_step(&Uac::Failed(2), 1000), UacStep::Undo("elevated start failed"));
    }

    #[test]
    fn cmdline() {
        assert_eq!(command_line("C:\\A B\\blink.exe", &s(&["--resident", "a b", "q\"x", "end\\"])), "\"C:\\A B\\blink.exe\" --resident \"a b\" \"q\\\"x\" end\\");
        assert_eq!(command_line("c.exe", &s(&["", "x y\\"])), "\"c.exe\" \"\" \"x y\\\\\"");
    }

    #[test]
    fn restart_as_admin_keeps_the_test_environment_and_tab() {
        let r = Restore { data_dir: Some(r"G:\Blink test\data dir".into()), multi: true, tab: Some(3), ..Default::default() };
        let args = admin_relaunch_args(&r, 4242);
        assert_eq!(args, s(&["--update-tab", "3", "--update-data-dir", r"G:\Blink test\data dir", "--update-multi", "--wait-pid", "4242"]));
        // what the elevated copy parses back after the C runtime splits the quoted line
        let line = join_args(&args);
        assert_eq!(line, r#"--update-tab 3 --update-data-dir "G:\Blink test\data dir" --update-multi --wait-pid 4242"#);
        let back = parse_restore(&args);
        assert_eq!((back.data_dir.as_deref(), back.multi, back.tab), (Some(r"G:\Blink test\data dir"), true, Some(3)));
        // nothing else rides along: no trial, no window placement, no free-form command
        assert_eq!((back.trial, back.show, back.place, back.maximized), (None, None, None, false));
        // a plain install has no environment to carry: only the wait
        let plain = admin_relaunch_args(&Restore::default(), 7);
        assert_eq!(plain, s(&["--wait-pid", "7"]));
        // a path with quotes and a trailing backslash survives the quoting
        let tricky = Restore { data_dir: Some("D:\\a \"b\"\\".into()), ..Default::default() };
        assert_eq!(join_args(&admin_relaunch_args(&tricky, 1)), "--update-data-dir \"D:\\a \\\"b\\\"\\\\\" --wait-pid 1");
    }

    #[test]
    fn manual_check_overrides_bad_only_for_the_source_hash() {
        let c = "c".repeat(64);
        let mut st = State { trial: "t".into(), trial_pid: 5, ready: "r".into(), updated_to: "u1".into(), updated_at: 9, rolled_back: "1a2b3c4".into(), rolled_back_at: 77, declined: "1a2b3c4".into(), declined_at: 55, ..Default::default() };
        st.mark_bad(A);
        st.mark_bad(B);
        // source = B (bad), running = c: B is staged again, A stays bad
        let r = manual_check(B, &c, None, "1a2b3c4", &mut st);
        assert_eq!(r, Manual::Stage { retry: true });
        assert_eq!(st.bad, vec![A.to_string()]);
        assert_eq!((st.rolled_back.as_str(), st.rolled_back_at, st.declined.as_str(), st.declined_at), ("", 0, "", 0));
        // trial / updated fields untouched
        assert_eq!((st.trial.as_str(), st.trial_pid, st.ready.as_str(), st.updated_to.as_str(), st.updated_at), ("t", 5, "r", "u1", 9));
        assert_eq!(manual_text(&r, "1a2b3c4"), "Update 1a2b3c4 was rolled back earlier; trying again");
    }

    #[test]
    fn manual_check_keeps_other_labels_and_plain_new_builds() {
        let c = "c".repeat(64);
        // rolled_back / declined name a different build: left alone
        let mut st = State { rolled_back: "old1111".into(), rolled_back_at: 3, declined: "old2222".into(), declined_at: 4, ..Default::default() };
        st.mark_bad(A);
        let r = manual_check(B, &c, None, "new9999", &mut st);
        assert_eq!(r, Manual::Stage { retry: false });
        assert_eq!(st.bad, vec![A.to_string()]);
        assert_eq!((st.rolled_back.as_str(), st.rolled_back_at, st.declined.as_str(), st.declined_at), ("old1111", 3, "old2222", 4));
        assert_eq!(manual_text(&r, "new9999"), "Update new9999 available \u{2014} Restart to apply");
        // bad entry belongs to another hash: that build's rolled_back label stays
        let mut st2 = State { rolled_back: "bad0001".into(), rolled_back_at: 8, ..Default::default() };
        st2.mark_bad(A);
        assert_eq!(manual_check(B, &c, None, "new9999", &mut st2), Manual::Stage { retry: false });
        assert_eq!(st2.rolled_back, "bad0001");
        // declined (UAC cancelled) for exactly this build: cleared, not bad
        let mut st3 = State { declined: "new9999".into(), declined_at: 2, ..Default::default() };
        assert_eq!(manual_check(B, &c, None, "new9999", &mut st3), Manual::Stage { retry: false });
        assert!(st3.declined.is_empty());
    }

    #[test]
    fn manual_check_no_change_when_up_to_date_or_staged() {
        let mut st = State { rolled_back: "x".into(), rolled_back_at: 1, declined: "x".into(), declined_at: 2, ..Default::default() };
        st.mark_bad(B);
        let before = st.clone();
        assert_eq!(manual_check(A, A, None, "x", &mut st), Manual::UpToDate);
        assert_eq!(st, before);
        assert_eq!(manual_text(&Manual::UpToDate, "1a2b3c4"), "Up to date (1a2b3c4)");
        assert_eq!(manual_check(A, B, Some(A), "x", &mut st), Manual::AlreadyStaged);
        assert_eq!(st, before);
        assert_eq!(manual_text(&Manual::AlreadyStaged, "x"), "Update x available \u{2014} Restart to apply");
    }

    #[test]
    fn manual_override_leaves_auto_rules_and_trial_outcome_alone() {
        let c = "c".repeat(64);
        let mut st = State::default();
        st.mark_bad(B);
        // the automatic check still refuses it
        assert_eq!(should_stage(B, &c, None, &st.bad), Stage::Nothing("known bad build"));
        assert_eq!(manual_check(B, &c, None, "l", &mut st), Manual::Stage { retry: true });
        // a retry that fails its trial is rolled back and marked bad again; auto refuses again
        st.trial = B.into();
        assert_eq!(trial_outcome(true, &st, B, 7, 1000), Trial::Rollback("exited before ready"));
        st.mark_bad(B);
        assert_eq!(should_stage(B, &c, None, &st.bad), Stage::Nothing("known bad build"));
        // the next manual check can override once more
        assert_eq!(manual_check(B, &c, None, "l", &mut st), Manual::Stage { retry: true });
        assert!(st.bad.is_empty());
    }

    #[test]
    fn manual_texts() {
        assert_eq!(manual_error("source not reachable"), "Update check failed: source not reachable");
        assert_eq!(rolled_back_note("1a2b3c4"), "Update 1a2b3c4 failed at start; rolled back \u{b7} click to try again");
    }

    #[test]
    fn ago_text() {
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(180), "3 min ago");
        assert_eq!(ago(7300), "2 h ago");
    }
}
