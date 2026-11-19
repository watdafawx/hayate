//! Pure parts of the taskbar access: the launch intents a Jump List task (or a
//! Nilesoft Shell menu item) starts Hayate with, the task definitions, when the
//! Jump List is (re)written, and the Nilesoft snippet. No I/O, so all of it is
//! unit-tested; `taskbar::mod` does the COM and file work.

use crate::setup::logic::Where;
use crate::update::logic::{join_args, norm_path, relaunch_args, Restore, Show};

/// Public one-shot command-line args (documented in docs/taskbar.md). They are
/// never replayed by an update relaunch (`update::logic::user_args` drops them).
pub const ARG_OPEN: &str = "--open";
pub const ARG_TAB: &str = "--tab";
pub const ARG_ADMIN: &str = "--restart-admin";
/// Headless: delete this app's Jump List, then exit (install.ps1 -Uninstall).
pub const ARG_REMOVE: &str = "--remove-jumplist";

/// What a taskbar launch asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
    /// bring the window up
    Open,
    /// bring it up on a tab (`Tab::restore_id`)
    Tab(usize),
    /// restart elevated: UAC, asked by Hayate itself
    RestartAdmin,
}

/// `--tab` names and the relaunch ids they stand for (`ui::Tab::restore_id`;
/// a test ties the two together).
pub const TAB_NAMES: [(&str, usize); 7] = [("vitals", 0), ("processes", 1), ("history", 3), ("startup", 4), ("services", 5), ("rules", 6), ("watch", 7)];

pub fn tab_id(name: &str) -> Option<usize> {
    TAB_NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name.trim())).map(|&(_, id)| id)
}

/// The intent in `args` (the arguments after the exe name). `--restart-admin`
/// wins over `--tab`, which wins over `--open`. A `--tab` with an unknown
/// name still opens the window. Everything else (`--resident`, the private
/// `--update-*` args, `-ToastActivated`) is not an intent.
pub fn parse_intent(args: &[String]) -> Option<Intent> {
    if args.iter().any(|a| a == ARG_ADMIN) {
        return Some(Intent::RestartAdmin);
    }
    if let Some(i) = args.iter().position(|a| a == ARG_TAB) {
        return Some(args.get(i + 1).and_then(|v| tab_id(v)).map_or(Intent::Open, Intent::Tab));
    }
    args.iter().any(|a| a == ARG_OPEN).then_some(Intent::Open)
}

/// The window message a second instance posts to the running Hayate
/// (`HayateShowWindow`): wparam 0 open, 1 is the toast click, 2 tab (lparam =
/// restore id). That message is let through the integrity filter (an elevated
/// Hayate must hear an unelevated launcher), so ANY process can post it, with no
/// way to tell who sent it: it may therefore only ever show the window or pick a
/// tab. `--restart-admin` has NO wire form: the process that was launched with
/// it asks UAC itself (`taskbar::restart_admin`), so no posted message can ever
/// cause a UAC prompt or a restart.
pub fn to_wire(i: Intent) -> Option<(usize, isize)> {
    match i {
        Intent::Open => Some((0, 0)),
        Intent::Tab(id) => Some((2, id as isize)),
        Intent::RestartAdmin => None,
    }
}

pub fn from_wire(wp: usize, lp: isize) -> Option<Intent> {
    match wp {
        0 => Some(Intent::Open),
        2 => usize::try_from(lp).ok().map(Intent::Tab),
        _ => None,
    }
}

// ---- tasks ---------------------------------------------------------------------

/// One Jump List task: a shell link to hayate.exe with these arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Task {
    pub title: &'static str,
    pub tip: &'static str,
    pub args: &'static [&'static str],
}

pub const TASKS: [Task; 4] = [
    Task { title: "Open Hayate", tip: "Show the Hayate window", args: &[ARG_OPEN] },
    Task { title: "Processes", tip: "Open Hayate on Processes", args: &[ARG_TAB, "processes"] },
    Task { title: "Vitals", tip: "Open Hayate on Vitals", args: &[ARG_TAB, "vitals"] },
    Task { title: "Restart as administrator", tip: "Restart Hayate with administrator rights (asks UAC)", args: &[ARG_ADMIN] },
];

impl Task {
    /// The shell link's Arguments field (the exe path is its own field).
    pub fn arguments(&self) -> String {
        join_args(&self.args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }
}

/// Bump when the task layout (not only titles/args) changes: forces a rewrite.
const LAYOUT: u32 = 1;

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// What a written Jump List stands for: app ID, exe path, every task. A change
/// rewrites it; the same value is left alone.
pub fn fingerprint(aumid: &str, exe: &str) -> String {
    let mut s = format!("{LAYOUT}|{aumid}|{}", norm_path(exe));
    for t in &TASKS {
        s.push_str(&format!("|{}|{}|{}", t.title, t.tip, t.arguments()));
    }
    format!("{:016x}", fnv(&s))
}

// ---- when to write ---------------------------------------------------------------

/// `jumplist.ini` in the data dir: what was last written (so a start does not
/// rewrite the shell's list every time).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub fingerprint: Option<String>,
    pub at: u64,
    /// the list was deleted (switch off / uninstall) and that was confirmed by the shell.
    /// Without it (no file, cache lost) a switched-off copy deletes once more: the
    /// cache must never be what decides whether an owned list is removed.
    pub removed: bool,
}

impl State {
    /// what a confirmed delete leaves
    pub fn after_delete() -> State {
        State { fingerprint: None, at: 0, removed: true }
    }
}

pub fn parse_state(text: &str) -> State {
    let mut s = State::default();
    for line in text.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "fingerprint" if v.len() == 16 && v.bytes().all(|c| c.is_ascii_hexdigit()) => s.fingerprint = Some(v.to_ascii_lowercase()),
            "at" => s.at = v.parse().unwrap_or(0),
            "removed" => s.removed = matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
            _ => {}
        }
    }
    s
}

pub fn write_state(s: &State) -> String {
    match &s.fingerprint {
        Some(f) => format!("; Hayate taskbar Jump List: what was last written (written by Hayate; safe to delete)\nfingerprint = {f}\nat = {}\n", s.at),
        None if s.removed => "; Hayate taskbar Jump List: removed (written by Hayate; safe to delete)\nremoved = true\n".to_string(),
        None => "; Hayate taskbar Jump List: nothing written (written by Hayate; safe to delete)\n".to_string(),
    }
}

/// A list the shell may have lost (cleared, profile reset) is rewritten after this long.
pub const REFRESH_SECS: u64 = 14 * 86400;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    Skip(&'static str),
    Write,
    /// switched off and not yet confirmed removed: take it out (once confirmed, stay quiet)
    Delete,
}

/// May this process touch the global Jump List of its app ID at all, to write
/// OR to remove it? `test` is the debug-build test AUMID: a disposable
/// identity, so that copy may register itself anywhere. Otherwise only an
/// installed, non-`HAYATE_MULTI` copy owns the real `Hayate.TaskManager` list: a
/// portable copy (a dev build, a rig worktree) or a test instance did not write
/// it, so it must not delete the installed copy's list either.
pub fn eligible(at: &Where, multi: bool, test: bool) -> Result<(), &'static str> {
    if test {
        return Ok(());
    }
    if multi {
        return Err("test instance (HAYATE_MULTI)");
    }
    if *at == Where::Portable {
        return Err("portable copy: nothing written or removed outside it");
    }
    Ok(())
}

pub fn plan(at: &Where, enabled: bool, multi: bool, test: bool, st: &State, fp: &str, now: u64) -> Plan {
    if let Err(why) = eligible(at, multi, test) {
        return Plan::Skip(why);
    }
    if !enabled {
        // delete unless the shell already confirmed one: a missing or unwritable
        // jumplist.ini must not let a switched-off copy keep its list
        return if st.fingerprint.is_none() && st.removed { Plan::Skip("switched off, list already removed") } else { Plan::Delete };
    }
    if st.fingerprint.as_deref() == Some(fp) && now < st.at.saturating_add(REFRESH_SECS) {
        return Plan::Skip("already written");
    }
    Plan::Write
}

// ---- admin launch ----------------------------------------------------------------

/// Arguments of the elevated copy a `--restart-admin` start asks UAC for: UAC
/// drops the environment, so HAYATE_DATA_DIR / HAYATE_MULTI travel as the private
/// args; shown in front. `replace` is the pid of the running unelevated Hayate it
/// replaces (`--wait-pid`: the elevated copy asks it to quit and waits for it).
/// Never contains `--restart-admin` (no prompt loop) and nothing a user typed.
pub fn elevate_args(data_dir: Option<String>, multi: bool, replace: Option<u32>) -> Vec<String> {
    let mut v = relaunch_args(&[], &Restore { show: Some(Show::Front), data_dir, multi, ..Default::default() });
    if let Some(pid) = replace {
        v.extend(["--replace-for-admin".to_string(), "--wait-pid".to_string(), pid.to_string()]);
    }
    v
}

/// What `--restart-admin` does in the process it launched, from what it finds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminRoute {
    /// no Hayate is running: UAC for a fresh elevated one
    ElevateNew,
    /// an unelevated Hayate is running: UAC for an elevated one that replaces it
    /// (the running one is left alone until UAC is accepted)
    ElevateReplace(u32),
    /// the running Hayate is elevated already, or its token cannot be read: no
    /// prompt, just show it
    Show,
}

/// `running`: the pid of a running Hayate and whether its token is elevated
/// (None = could not tell).
pub fn admin_route(running: Option<(u32, Option<bool>)>) -> AdminRoute {
    match running {
        None => AdminRoute::ElevateNew,
        Some((pid, Some(false))) => AdminRoute::ElevateReplace(pid),
        Some(_) => AdminRoute::Show,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Elevated {
    Started,
    /// the user answered the UAC prompt with No: nothing changes, nothing is retried
    Cancelled,
    Failed(String),
}

/// `setup::runas` reports the user's refusal as the text "cancelled".
pub fn classify(r: Result<(), String>) -> Elevated {
    match r {
        Ok(()) => Elevated::Started,
        Err(e) if e == "cancelled" => Elevated::Cancelled,
        Err(e) => Elevated::Failed(e),
    }
}

// ---- Nilesoft Shell ----------------------------------------------------------------

/// Characters Nilesoft would read as syntax inside a single-quoted string
/// (`'` ends it, `@` and `$` start expressions, `"` closes the quoted exe; `%`
/// is refused as well, conservatively, in case it is expanded). A path with one
/// is refused, never escaped by guess.
fn nilesoft_bad(c: char) -> bool {
    matches!(c, '\'' | '@' | '$' | '"' | '%') || c.is_control()
}

pub fn nilesoft_path_ok(p: &str) -> bool {
    !p.is_empty() && !p.chars().any(nilesoft_bad)
}

/// The offending characters of `p`, readable: `' @ $`, control characters as `?`.
fn bad_chars(p: &str) -> String {
    let mut seen: Vec<char> = Vec::new();
    for c in p.chars().filter(|&c| nilesoft_bad(c)) {
        let c = if c.is_control() { '?' } else { c };
        if !seen.contains(&c) {
            seen.push(c);
        }
    }
    seen.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(" ")
}

/// Why `p` cannot go into the snippet, as text the user can act on. `what`
/// names the path ("The path of hayate.exe", "Hayate's data folder").
fn path_problem(what: &str, p: &str, fix: &str) -> String {
    if p.is_empty() {
        return format!("{what} is empty. {fix}");
    }
    format!("{what} ({p}) contains {}, which Nilesoft Shell would misread. {fix}", bad_chars(p))
}

/// Check BOTH paths a saved snippet depends on, before anything is written:
/// `exe` goes into the menu items, `import_file` is the path the user must put
/// in `import '...'` in shell.nss (the saved snippet inside the data dir, which
/// `HAYATE_DATA_DIR` can move anywhere). Ok: the snippet text.
pub fn nilesoft_save(exe: &str, import_file: &str) -> Result<String, String> {
    let text = nilesoft_snippet(exe)?;
    if !nilesoft_path_ok(import_file) {
        return Err(path_problem(
            "Hayate's data folder, where the snippet would be saved,",
            import_file,
            "Nothing was saved. Use Copy snippet and save the text yourself under a plain path, or point HAYATE_DATA_DIR at one.",
        ));
    }
    Ok(text)
}

/// `hayate-taskbar.nss` for Nilesoft Shell: one menu on the taskbar background
/// (`type="taskbar"`), the same four commands as the Jump List. Syntax follows
/// the `imports\taskbar.nss` shipped with Nilesoft Shell.
pub fn nilesoft_snippet(exe: &str) -> Result<String, String> {
    if !nilesoft_path_ok(exe) {
        return Err(path_problem("The path of hayate.exe", exe, "Install Hayate to a folder whose path has none of them."));
    }
    let mut s = String::new();
    s.push_str("// Hayate: entry for the taskbar background menu of Nilesoft Shell (made by Hayate, safe to delete).\n");
    s.push_str("// To use it, add this line at the top level of your shell.nss, then Shell > manager > Register, Restart Explorer:\n");
    s.push_str("//   import 'PATH OF THIS FILE'\n");
    s.push_str("// Hayate does not edit shell.nss. Each item starts hayate.exe with a public one-shot argument.\n");
    s.push_str("menu(type=\"taskbar\" title=\"Hayate\" image=\\uE9D9)\n{\n");
    for t in &TASKS {
        s.push_str(&format!("\titem(title=\"{}\" cmd='\"{exe}\"' args='{}')\n", t.title, t.arguments()));
    }
    s.push_str("}\n");
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    const EXE: &str = r"C:\Users\a b\AppData\Local\Programs\Hayate\hayate.exe";

    #[test]
    fn intents_from_the_command_line() {
        assert_eq!(parse_intent(&s(&[])), None);
        assert_eq!(parse_intent(&s(&["--resident"])), None);
        assert_eq!(parse_intent(&s(&["-ToastActivated", "-Embedding"])), None);
        assert_eq!(parse_intent(&s(&["--update-tab", "3", "--update-show", "front"])), None);
        assert_eq!(parse_intent(&s(&["--open"])), Some(Intent::Open));
        assert_eq!(parse_intent(&s(&["--tab", "processes"])), Some(Intent::Tab(1)));
        assert_eq!(parse_intent(&s(&["--tab", "Vitals"])), Some(Intent::Tab(0)));
        assert_eq!(parse_intent(&s(&["--tab", " WATCH "])), Some(Intent::Tab(7)));
        // an unknown or missing tab name still opens the window, never guesses a tab
        assert_eq!(parse_intent(&s(&["--tab", "nope"])), Some(Intent::Open));
        assert_eq!(parse_intent(&s(&["--tab"])), Some(Intent::Open));
        assert_eq!(parse_intent(&s(&["--restart-admin"])), Some(Intent::RestartAdmin));
        // precedence: admin > tab > open
        assert_eq!(parse_intent(&s(&["--open", "--tab", "rules", "--restart-admin"])), Some(Intent::RestartAdmin));
        assert_eq!(parse_intent(&s(&["--open", "--tab", "rules"])), Some(Intent::Tab(6)));
        // a value that merely looks like a flag is not one
        assert_eq!(parse_intent(&s(&["--update-data-dir", "--restart-admin-x"])), None);
    }

    #[test]
    fn tab_names_match_the_ui_tabs() {
        for (tab, _, label) in crate::ui::TABS {
            assert_eq!(tab_id(&label.to_lowercase()), Some(tab.restore_id()), "{label}");
        }
        assert_eq!(TAB_NAMES.len(), crate::ui::TABS.len());
        // every id the CLI can name is a real tab
        for (_, id) in TAB_NAMES {
            assert!(crate::ui::Tab::from_restore_id(id).is_some());
        }
    }

    #[test]
    fn wire_round_trip() {
        for i in [Intent::Open, Intent::Tab(0), Intent::Tab(7)] {
            let (wp, lp) = to_wire(i).unwrap();
            assert_eq!(from_wire(wp, lp), Some(i));
        }
        // 1 is the toast click (setup::forward), never an intent
        assert_eq!(from_wire(1, 5), None);
        assert_eq!(from_wire(9, 0), None);
        assert_eq!(from_wire(2, -1), None);
    }

    /// F2: the message any process can post must never be able to start UAC.
    #[test]
    fn no_posted_message_can_ask_for_admin() {
        assert_eq!(to_wire(Intent::RestartAdmin), None);
        // the old wire value 3, and every other value, is show/tab at most
        for wp in 0..64usize {
            for lp in [-1isize, 0, 1, 7, 8, 1 << 40] {
                assert_ne!(from_wire(wp, lp), Some(Intent::RestartAdmin), "wp {wp} lp {lp}");
            }
        }
        assert_eq!(from_wire(3, 0), None);
    }

    #[test]
    fn admin_restart_routes() {
        assert_eq!(admin_route(None), AdminRoute::ElevateNew);
        assert_eq!(admin_route(Some((10, Some(false)))), AdminRoute::ElevateReplace(10));
        // already elevated, or unknown: never prompt for it
        assert_eq!(admin_route(Some((10, Some(true)))), AdminRoute::Show);
        assert_eq!(admin_route(Some((10, None))), AdminRoute::Show);
    }

    #[test]
    fn task_definitions() {
        let titles: Vec<_> = TASKS.iter().map(|t| t.title).collect();
        assert_eq!(titles, ["Open Hayate", "Processes", "Vitals", "Restart as administrator"]);
        let intents: Vec<_> = TASKS.iter().map(|t| parse_intent(&t.args.iter().map(|a| a.to_string()).collect::<Vec<_>>())).collect();
        assert_eq!(intents, [Some(Intent::Open), Some(Intent::Tab(1)), Some(Intent::Tab(0)), Some(Intent::RestartAdmin)]);
        assert_eq!(TASKS[1].arguments(), "--tab processes");
        assert_eq!(TASKS[3].arguments(), "--restart-admin");
        // only the last one asks for UAC, and only by naming it; nothing else rides along
        for t in &TASKS[..3] {
            assert!(!t.args.contains(&ARG_ADMIN));
        }
        for t in &TASKS {
            assert!(t.title.chars().count() <= 32, "Jump List titles stay short");
            for a in t.args {
                assert!(!a.starts_with("--update-") && *a != "--wait-pid" && !a.eq_ignore_ascii_case("-ToastActivated"), "{a}");
            }
        }
        // none of the one-shot args may survive an update relaunch (a replayed --restart-admin would ask UAC again)
        let replay = crate::update::logic::relaunch_args(&s(&["--resident", "--open", "--tab", "processes", "--restart-admin"]), &Restore::default());
        assert_eq!(replay, s(&["--resident"]));
    }

    #[test]
    fn fingerprint_tracks_path_and_identity() {
        let a = fingerprint("Hayate.TaskManager", EXE);
        assert_eq!(a.len(), 16);
        assert_eq!(a, fingerprint("Hayate.TaskManager", &EXE.to_uppercase()));
        assert_eq!(a, fingerprint("Hayate.TaskManager", &EXE.replace('\\', "/")));
        assert_ne!(a, fingerprint("Hayate.TaskManager", r"C:\Program Files\Hayate\hayate.exe"));
        assert_ne!(a, fingerprint("Hayate.TaskManager.Test1", EXE));
    }

    #[test]
    fn state_round_trip() {
        let st = State { fingerprint: Some("00ff00ff00ff00ff".into()), at: 1_700_000_000, removed: false };
        assert_eq!(parse_state(&write_state(&st)), st);
        assert_eq!(parse_state(&write_state(&State::after_delete())), State::after_delete());
        assert!(parse_state(&write_state(&State::after_delete())).removed);
        assert!(!parse_state(&write_state(&State::default())).removed);
        assert_eq!(parse_state(&write_state(&State::default())), State::default());
        assert_eq!(parse_state("fingerprint = zz\nat = x\n"), State::default());
        assert_eq!(parse_state(""), State::default());
    }

    #[test]
    fn when_the_list_is_written() {
        let fp = "aaaaaaaaaaaaaaaa";
        let none = State::default();
        let cur = State { fingerprint: Some(fp.into()), at: 1000, removed: false };
        let now = 2000;
        let user = Where::User;
        // never from a portable copy or a test instance, written or not
        assert!(matches!(plan(&Where::Portable, true, false, false, &none, fp, now), Plan::Skip(_)));
        assert!(matches!(plan(&Where::Portable, false, false, false, &cur, fp, now), Plan::Skip(_)));
        assert!(matches!(plan(&user, true, true, false, &none, fp, now), Plan::Skip(_)));
        // installed: first time, changed path, or old list
        assert_eq!(plan(&user, true, false, false, &none, fp, now), Plan::Write);
        assert_eq!(plan(&Where::Machine, true, false, false, &none, fp, now), Plan::Write);
        assert_eq!(plan(&user, true, false, false, &cur, "bbbbbbbbbbbbbbbb", now), Plan::Write);
        assert_eq!(plan(&user, true, false, false, &cur, fp, 1000 + REFRESH_SECS), Plan::Write);
        // unchanged and recent: the shell is left alone
        assert!(matches!(plan(&user, true, false, false, &cur, fp, now), Plan::Skip(_)));
        // switched off: remove once, then stay quiet
        assert_eq!(plan(&user, false, false, false, &cur, fp, now), Plan::Delete);
        // F5: no cache at all (never written here, file lost, cache write failed): still delete once, then stay quiet
        assert_eq!(plan(&user, false, false, false, &none, fp, now), Plan::Delete);
        assert!(matches!(plan(&user, false, false, false, &State::after_delete(), fp, now), Plan::Skip(_)));
        // a failed delete leaves the old cache, so the next try deletes again
        assert_eq!(plan(&user, false, false, false, &cur, fp, now + 1), Plan::Delete);
        // switching back on after a confirmed delete writes again
        assert_eq!(plan(&user, true, false, false, &State::after_delete(), fp, now), Plan::Write);
        // the disposable test identity may register from anywhere
        assert_eq!(plan(&Where::Portable, true, true, true, &none, fp, now), Plan::Write);
    }

    #[test]
    fn admin_launch_args() {
        assert_eq!(elevate_args(None, false, None), s(&["--update-show", "front"]));
        let a = elevate_args(Some(r"G:\Hayate test\data dir".into()), true, None);
        assert_eq!(a, s(&["--update-show", "front", "--update-data-dir", r"G:\Hayate test\data dir", "--update-multi"]));
        let line = join_args(&a);
        assert_eq!(line, r#"--update-show front --update-data-dir "G:\Hayate test\data dir" --update-multi"#);
        // the elevated copy must not ask again
        assert!(parse_intent(&a).is_none());
        assert!(!line.contains("restart-admin"));
        // replacing a running Hayate: the elevated copy is told whom to wait for
        let r = elevate_args(None, false, Some(4242));
        assert_eq!(r, s(&["--update-show", "front", "--replace-for-admin", "--wait-pid", "4242"]));
        assert!(crate::update::logic::user_args(&r).is_empty());
        assert!(parse_intent(&r).is_none());
    }

    #[test]
    fn uac_answers() {
        assert_eq!(classify(Ok(())), Elevated::Started);
        // No on the prompt is a decision: no fallback start, no retry
        assert_eq!(classify(Err("cancelled".into())), Elevated::Cancelled);
        assert_eq!(classify(Err("The system cannot find the file".into())), Elevated::Failed("The system cannot find the file".into()));
    }

    #[test]
    fn nilesoft_snippet_is_quoted_and_complete() {
        let n = nilesoft_snippet(EXE).unwrap();
        assert!(n.contains("menu(type=\"taskbar\" title=\"Hayate\""));
        // the exe is quoted inside the single-quoted cmd, spaces and all
        assert!(n.contains(&format!("cmd='\"{EXE}\"'")));
        for t in &TASKS {
            assert!(n.contains(&format!("item(title=\"{}\" cmd='\"{EXE}\"' args='{}')", t.title, t.arguments())), "{}", t.title);
        }
        // braces balance, one menu
        assert_eq!(n.matches('{').count(), 1);
        assert_eq!(n.matches('}').count(), 1);
        assert_eq!(n.matches("menu(").count(), 1);
        // it never touches the user's own config
        assert!(!n.contains("remove(") && !n.contains("modify("));
        assert!(nilesoft_snippet(r"C:\Users\o'neil\hayate.exe").is_err());
        assert!(nilesoft_snippet(r"C:\Users\me@x\hayate.exe").is_err());
        assert!(nilesoft_snippet(r"C:\$Recycle\hayate.exe").is_err());
        assert!(nilesoft_snippet("").is_err());
        assert!(nilesoft_snippet("C:\\a\nb\\hayate.exe").is_err());
        assert!(nilesoft_snippet(r"C:\100%\hayate.exe").is_err());
    }

    #[test]
    fn eligibility_is_shared_by_write_and_remove() {
        assert!(eligible(&Where::User, false, false).is_ok());
        assert!(eligible(&Where::Machine, false, false).is_ok());
        assert!(eligible(&Where::Portable, false, false).is_err());
        assert!(eligible(&Where::User, true, false).is_err());
        assert!(eligible(&Where::Portable, true, false).is_err());
        // the disposable debug identity may touch its own list from anywhere
        assert!(eligible(&Where::Portable, true, true).is_ok());
        // plan agrees: an ineligible copy never writes or deletes, whatever jumplist.ini says
        let cur = State { fingerprint: Some("aaaaaaaaaaaaaaaa".into()), at: 1, removed: false };
        for (at, multi) in [(Where::Portable, false), (Where::User, true), (Where::Portable, true)] {
            assert!(matches!(plan(&at, true, multi, false, &cur, "bbbbbbbbbbbbbbbb", 5), Plan::Skip(_)));
            assert!(matches!(plan(&at, false, multi, false, &cur, "bbbbbbbbbbbbbbbb", 5), Plan::Skip(_)));
        }
    }

    #[test]
    fn save_checks_the_import_path_too() {
        let good_import = r"C:\Users\a b\AppData\Local\Hayate\nilesoft\hayate-taskbar.nss";
        assert!(nilesoft_save(EXE, good_import).is_ok());
        // a data dir from HAYATE_DATA_DIR with syntax characters: refused before any write
        for bad in [r"C:\o'neil\nilesoft\hayate-taskbar.nss", r"D:\me@home\hayate-taskbar.nss", r"E:\$data\hayate-taskbar.nss", "F:\\q\"x\\hayate-taskbar.nss", r"G:\50%\hayate-taskbar.nss", "H:\\a\nb\\hayate-taskbar.nss", ""] {
            let e = nilesoft_save(EXE, bad).unwrap_err();
            assert!(e.contains("Nothing was saved"), "{bad:?}: {e}");
        }
        // the error names the data folder and the offending character, and a way out
        let e = nilesoft_save(EXE, r"C:\o'neil\nilesoft\hayate-taskbar.nss").unwrap_err();
        assert!(e.contains(r"C:\o'neil") && e.contains('\'') && e.contains("HAYATE_DATA_DIR") && e.contains("Copy snippet"), "{e}");
        // the exe is checked as before, and named as such
        let e = nilesoft_save(r"C:\o'neil\hayate.exe", good_import).unwrap_err();
        assert!(e.contains("hayate.exe") && e.contains('\''), "{e}");
        // both bad: the exe error comes first, nothing is produced
        assert!(nilesoft_save(r"C:\o'neil\hayate.exe", r"C:\p@\x.nss").is_err());
        assert_eq!(bad_chars("a'b@c'd\n"), "' @ ?");
    }
}
