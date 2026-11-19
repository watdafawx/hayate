//! The process context menu, shared by the Processes list and every Vitals
//! "TOP BY ..." list.
//!
//! A menu is built from a [`Target`]: what the user right-clicked, captured when
//! the menu opens. It is a value: it holds identities (pid + create time), never
//! row indexes, and nothing in it follows the list afterwards. Whatever the lists
//! do while the menu is up (re-rank, re-sort, drop or reuse a PID, gain or lose
//! group members), the chosen command goes to these processes, and each one is
//! re-verified at the OS (`osutil::open_same`) just before it is touched.
//!
//! Two scopes are kept apart, and the menu says which is which:
//! - GROUP actions (End task, End process tree, Priority this time, View rule(s))
//!   reach every captured member of the row;
//! - PROCESS actions (efficiency, CPU affinity / sets, trim, restart, the per-exe
//!   rules, file location, copies) reach ONE process: the representative. For a
//!   group row of more than one process a caption names it, and nothing falls back
//!   to another process when it has left.
//!
//! The menu itself is data ([`build`]), so what each list offers is unit-tested;
//! `win::row_menu` only draws it and runs the chosen id.

use crate::rules::Actions;

pub type Member = (u32, i64, String);

/// Where the menu was opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Processes,
    /// a Vitals "TOP BY ..." list
    Top,
}

/// The one process the process actions reach.
#[derive(Clone, Debug, PartialEq)]
pub struct Rep {
    pub pid: u32,
    pub create: i64,
    pub name: String,
    pub path: Option<String>,
    pub cmd: Option<String>,
}

/// Where "Show in Processes" goes: the row to select there, and the group to expand to reach it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShowIn {
    /// (pid, create time, is a group row)
    pub select: (u32, i64, bool),
    pub expand: Option<(u32, i64)>,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub origin: Origin,
    /// what the row is called (app / group name, or the process name)
    pub label: String,
    /// the row stands for a group (an app, a Processes group row)
    pub group: bool,
    /// process-specific actions go here; None when it left (they are then off)
    pub rep: Option<Rep>,
    /// group actions go here: the members the row stood for, still running when the menu opened
    pub members: Vec<Member>,
    pub show: Option<ShowIn>,
}

impl Target {
    /// More than one process: the menu then separates group actions from process actions.
    pub fn multi(&self) -> bool {
        self.members.len() > 1
    }
}

// ---- command ids (also the dispatch table of `win::row_menu`) ----

pub const ID_END: usize = 1;
pub const ID_END_TREE: usize = 2;
pub const ID_LOCATE: usize = 3;
pub const ID_COPY_CMD: usize = 4;
pub const ID_COPY_PATH: usize = 5;
pub const ID_COPY_PID: usize = 6;
pub const ID_PROPS: usize = 7;
pub const ID_EFF_ONCE: usize = 8;
pub const ID_TRIM: usize = 9;
pub const ID_RESTART: usize = 10;
pub const ID_RESTART_ADMIN: usize = 11;
pub const ID_CPU_AFF: usize = 12;
pub const ID_CPU_SETS: usize = 13;
pub const ID_SHOW: usize = 14;
pub const ID_DISALLOW: usize = 15;
pub const ID_PRIO: usize = 20;
pub const ID_R_EFF: usize = 40;
pub const ID_R_IO: usize = 41;
pub const ID_R_AWAKE: usize = 42;
pub const ID_R_REMOVE: usize = 43;
pub const ID_R_SHOW: usize = 44;
pub const ID_R_BOOST: usize = 45;
pub const ID_R_VIEW: usize = 46;
pub const ID_R_PRIO: usize = 50;
pub const ID_R_SETS: usize = 60;
pub const ID_R_AFF: usize = 70;
pub const ID_R_POWER: usize = 80;
/// disabled lines (captions, notes): never dispatched
pub const ID_NOTE: usize = 99;

/// What the menu reads from the machine, so building it needs no OS call.
pub struct Env {
    /// the rules engine is running (rule items and "View rule(s)" are offered)
    pub rules: bool,
    /// the saved rule whose pattern is the representative's exe name: its actions
    pub rule: Option<Actions>,
    pub presets: Vec<crate::rules::Preset>,
    pub plans: Vec<crate::power::Plan>,
    /// CPU selection works on this PC; Err(processor groups) when it does not
    pub cpu: Result<(), u16>,
    /// the CCD presets are switched off although this PC has two CCDs
    pub ccd_hint: bool,
    pub elevated: bool,
    /// Disallow running may be set for the representative; Err(why not) turns the item off
    pub disallow: Result<(), String>,
}

/// One menu line, in draw order.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Item { id: usize, text: String, checked: bool, enabled: bool },
    Sep,
    Sub { text: String, items: Vec<Entry> },
}

fn item(id: usize, text: impl Into<String>, enabled: bool) -> Entry {
    Entry::Item { id, text: text.into(), checked: false, enabled }
}
fn chk(id: usize, text: impl Into<String>, checked: bool) -> Entry {
    Entry::Item { id, text: text.into(), checked, enabled: true }
}
fn note(text: impl Into<String>) -> Entry {
    item(ID_NOTE, text, false)
}

/// Caption above the group actions of a row of several processes.
pub fn group_caption(t: &Target) -> String {
    format!("{} · {} processes", t.label, t.members.len())
}

/// Caption above the process actions when they reach one process of a group, or none.
pub fn rep_caption(t: &Target) -> String {
    match &t.rep {
        Some(r) => format!("This process only: {} · PID {}", r.name, r.pid),
        None => "This process only: the main process has exited".to_string(),
    }
}

pub fn build(t: &Target, env: &Env) -> Vec<Entry> {
    let n = t.members.len();
    let multi = t.multi();
    let rep = t.rep.as_ref();
    let pid = rep.map(|r| r.pid);
    let path = rep.and_then(|r| r.path.as_deref());
    let mut m: Vec<Entry> = Vec::new();

    // ---- group actions: every captured member ----
    if multi {
        m.push(note(group_caption(t)));
    }
    m.push(item(ID_END, if multi { format!("End task (all {n} processes)") } else { "End task".to_string() }, n > 0));
    m.push(item(ID_END_TREE, if multi { format!("End process tree ({n} processes + their child processes)") } else { "End process tree".to_string() }, n > 0));
    m.push(Entry::Sep);
    let prio: Vec<Entry> = ["Realtime", "High", "Above normal", "Normal", "Below normal", "Low"].iter().enumerate().map(|(i, name)| item(ID_PRIO + i, *name, true)).collect();
    m.push(Entry::Sub { text: if multi { format!("Priority, all {n} processes (this time)") } else { "Priority (this time)".to_string() }, items: prio });
    if multi {
        m.push(item(ID_R_VIEW, format!("View rule(s) for all {n} processes…"), env.rules && n > 0));
    }

    // ---- process actions: the representative ----
    if multi || rep.is_none() {
        m.push(Entry::Sep);
        m.push(note(rep_caption(t)));
    }
    m.push(item(ID_EFF_ONCE, "Efficiency mode (this time)", pid.is_some()));
    // CPU affinity / CPU sets: a panel (custom CPU picks, current state, reset)
    match &env.cpu {
        Ok(()) => {
            let ok = pid.is_some_and(|p| p > 4) && rep.is_some_and(|r| r.create != 0);
            m.push(item(ID_CPU_AFF, "CPU affinity… (hard limit)", ok));
            m.push(item(ID_CPU_SETS, "CPU sets… (scheduler selection)", ok));
        }
        Err(groups) => m.push(item(ID_CPU_AFF, format!("CPU affinity / sets: off ({groups} processor groups)"), false)),
    }

    // Rules: saved per exe name, applied on every start
    if let (Some(r), true) = (rep, env.rules) {
        let ra = env.rule.clone().unwrap_or_default();
        let saved = env.rule.is_some();
        let mut rm: Vec<Entry> = Vec::new();
        let rp: Vec<Entry> = crate::rules::Priority::ALL.iter().enumerate().rev().map(|(i, p)| chk(ID_R_PRIO + i, p.label(), ra.priority == Some(*p))).collect();
        rm.push(Entry::Sub { text: "Always: priority".into(), items: rp });
        rm.push(chk(ID_R_EFF, "Always: efficiency mode", ra.efficiency == Some(true)));
        rm.push(chk(ID_R_IO, "Always: I/O priority low", ra.io == Some(crate::rules::IoPriority::Low)));
        rm.push(chk(ID_R_BOOST, "Always: disable thread boost", ra.boost == Some(false)));
        rm.push(chk(ID_R_AWAKE, "Always: keep display awake while running", ra.keep_awake.is_some()));
        if !env.plans.is_empty() {
            let pm: Vec<Entry> = env.plans.iter().enumerate().take(10).map(|(i, p)| chk(ID_R_POWER + i, p.name.clone(), ra.power_plan == Some(p.guid))).collect();
            rm.push(Entry::Sub { text: "Always: power plan while running".into(), items: pm });
        }
        if !env.presets.is_empty() {
            rm.push(Entry::Sep);
            for (i, p) in env.presets.iter().enumerate().take(8) {
                rm.push(chk(ID_R_SETS + i, format!("Always: CPU sets → {}", p.label), ra.cpu_sets == Some(p.mask)));
            }
            for (i, p) in env.presets.iter().enumerate().take(8) {
                rm.push(chk(ID_R_AFF + i, format!("Always: affinity → {}", p.label), ra.affinity == Some(p.mask)));
            }
        }
        if env.ccd_hint {
            rm.push(Entry::Sep);
            rm.push(note("CCD presets are off: Settings → CPU turns them on"));
        }
        rm.push(Entry::Sep);
        rm.push(item(ID_R_SHOW, "Show in Rules tab", saved));
        rm.push(item(ID_R_REMOVE, "Remove rule", saved));
        let dot = if saved { "  ●" } else { "" };
        // a rule is saved per exe name: for a group, say whose exe, and that it is not just this group
        let title = if multi { format!("Rules for {} (every {}){dot}", r.name, r.name) } else { format!("Rules for {}{dot}", r.name) };
        m.push(Entry::Sub { text: title, items: rm });
    }
    // Disallow: ends the program after it starts. Scope is the exe (every copy), never a group or "this PID"
    if env.rules {
        let exe = rep.map(|r| r.name.as_str()).unwrap_or("");
        match (&env.disallow, rep) {
            (Ok(()), Some(r)) => m.push(item(ID_DISALLOW, format!("Disallow running… (every {exe})"), r.create != 0 && r.path.is_some())),
            (Err(why), _) => m.push(item(ID_DISALLOW, format!("Disallow running: {why}"), false)),
            (Ok(()), None) => m.push(item(ID_DISALLOW, "Disallow running…", false)),
        }
    }
    // read-only: which saved rules match this process (or each member of a group), and why
    if !multi {
        m.push(item(ID_R_VIEW, "View rule(s)…", env.rules && n > 0));
    }
    m.push(Entry::Sep);
    m.push(item(ID_TRIM, "Trim memory", pid.is_some()));
    m.push(item(ID_RESTART, "Restart", pid.is_some() && path.is_some()));
    m.push(item(ID_RESTART_ADMIN, "Restart as admin", pid.is_some() && path.is_some() && env.elevated));
    m.push(Entry::Sep);
    m.push(item(ID_LOCATE, "Open file location", path.is_some()));
    m.push(item(ID_PROPS, "Properties", path.is_some()));
    m.push(item(ID_COPY_CMD, "Copy command line", rep.is_some_and(|r| r.cmd.is_some())));
    m.push(item(ID_COPY_PATH, "Copy path", path.is_some()));
    m.push(item(ID_COPY_PID, "Copy PID", pid.is_some()));

    // a Top list shows no tree to pick a member from: this opens the real one
    if t.origin == Origin::Top {
        m.push(Entry::Sep);
        m.push(item(ID_SHOW, if multi { "Show in Processes (pick a process)" } else { "Show in Processes" }, t.show.is_some()));
    }
    m
}

/// Every command id of a menu, in order (submenus included).
#[cfg(test)]
pub fn ids(entries: &[Entry]) -> Vec<usize> {
    let mut v = Vec::new();
    for e in entries {
        match e {
            Entry::Item { id, .. } if *id != ID_NOTE => v.push(*id),
            Entry::Sub { items, .. } => v.extend(ids(items)),
            _ => {}
        }
    }
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn rep(pid: u32, name: &str) -> Rep {
        Rep { pid, create: pid as i64 * 10, name: name.into(), path: Some(format!("C:\\x\\{name}")), cmd: Some(format!("{name} --x")) }
    }
    pub fn target(origin: Origin, group: bool, n: u32) -> Target {
        let members: Vec<Member> = (0..n).map(|i| (100 + i, (100 + i) as i64 * 10, "a.exe".to_string())).collect();
        Target { origin, label: "App".into(), group, rep: Some(rep(100, "a.exe")), members, show: Some(ShowIn { select: (100, 1000, true), expand: None }) }
    }
    pub fn env() -> Env {
        Env { rules: true, rule: None, presets: vec![], plans: vec![], cpu: Ok(()), ccd_hint: false, elevated: false, disallow: Ok(()) }
    }

    fn find(es: &[Entry], id: usize) -> Option<Entry> {
        for e in es {
            match e {
                Entry::Item { id: i, .. } if *i == id => return Some(e.clone()),
                Entry::Sub { items, .. } => {
                    if let Some(x) = find(items, id) {
                        return Some(x);
                    }
                }
                _ => {}
            }
        }
        None
    }
    fn text(es: &[Entry], id: usize) -> String {
        match find(es, id) {
            Some(Entry::Item { text, .. }) => text,
            _ => panic!("no item {id}"),
        }
    }
    fn enabled(es: &[Entry], id: usize) -> bool {
        matches!(find(es, id), Some(Entry::Item { enabled: true, .. }))
    }

    #[test]
    fn top_and_processes_offer_the_same_commands() {
        // single process, single-member app, several-member app: the same builder, the same commands
        for (group, n) in [(false, 1), (true, 1), (true, 4)] {
            let mut env = env();
            env.rule = Some(Actions::default());
            env.presets = vec![crate::rules::Preset { label: "CCD0".into(), mask: 0xff }];
            env.plans = vec![crate::power::Plan { guid: windows::core::GUID::zeroed(), name: "Balanced".into() }];
            let p = build(&target(Origin::Processes, group, n), &env);
            let t = build(&target(Origin::Top, group, n), &env);
            let (pi, mut ti) = (ids(&p), ids(&t));
            assert_eq!(ti.pop(), Some(ID_SHOW), "Show in Processes is the one extra, last");
            assert_eq!(pi, ti, "group={group} n={n}");
            // and every one of them is wired in the dispatch: no id appears twice
            let mut u = pi.clone();
            u.sort();
            u.dedup();
            assert_eq!(u.len(), pi.len());
        }
    }

    #[test]
    fn single_process_menu_keeps_the_processes_wording() {
        let es = build(&target(Origin::Processes, false, 1), &env());
        assert_eq!(text(&es, ID_END), "End task");
        assert_eq!(text(&es, ID_END_TREE), "End process tree");
        assert_eq!(text(&es, ID_R_VIEW), "View rule(s)…");
        assert!(matches!(&es[0], Entry::Item { id: ID_END, .. }), "no caption on a single process");
        assert!(!es.iter().any(|e| matches!(e, Entry::Item { id: ID_SHOW, .. })));
        let top = build(&target(Origin::Top, false, 1), &env());
        assert_eq!(text(&top, ID_SHOW), "Show in Processes");
    }

    #[test]
    fn group_menu_separates_group_scope_from_one_process() {
        let es = build(&target(Origin::Top, true, 4), &env());
        let first = match &es[0] {
            Entry::Item { text, enabled, .. } => (text.clone(), *enabled),
            _ => panic!(),
        };
        assert_eq!(first, ("App · 4 processes".to_string(), false), "group caption, not clickable");
        assert_eq!(text(&es, ID_END), "End task (all 4 processes)");
        assert!(text(&es, ID_END_TREE).starts_with("End process tree (4 processes"));
        assert!(text(&es, ID_R_VIEW).contains("all 4 processes"));
        let sub = es.iter().find_map(|e| match e {
            Entry::Sub { text, .. } if text.starts_with("Priority") => Some(text.clone()),
            _ => None,
        });
        assert_eq!(sub.as_deref(), Some("Priority, all 4 processes (this time)"));
        // the process actions are announced as one process, by name and PID
        let cap = es.iter().find_map(|e| match e {
            Entry::Item { id: ID_NOTE, text, .. } if text.starts_with("This process only") => Some(text.clone()),
            _ => None,
        });
        assert_eq!(cap.as_deref(), Some("This process only: a.exe · PID 100"));
        let rules = es.iter().find_map(|e| match e {
            Entry::Sub { text, .. } if text.starts_with("Rules for") => Some(text.clone()),
            _ => None,
        });
        assert_eq!(rules.as_deref(), Some("Rules for a.exe (every a.exe)"), "per-exe scope is explicit");
        assert_eq!(text(&es, ID_SHOW), "Show in Processes (pick a process)");
    }

    #[test]
    fn exited_representative_turns_process_actions_off_and_never_falls_back() {
        for n in [1, 3] {
            let mut t = target(Origin::Top, true, n);
            t.rep = None;
            let es = build(&t, &env());
            for id in [ID_EFF_ONCE, ID_CPU_AFF, ID_CPU_SETS, ID_TRIM, ID_RESTART, ID_RESTART_ADMIN, ID_LOCATE, ID_PROPS, ID_COPY_CMD, ID_COPY_PATH, ID_COPY_PID] {
                assert!(!enabled(&es, id), "id {id} n={n}");
            }
            assert!(find(&es, ID_R_PRIO).is_none(), "no per-exe rule for a process that is gone");
            assert!(enabled(&es, ID_END) && enabled(&es, ID_END_TREE), "group actions still reach the members");
            assert!(es.iter().any(|e| matches!(e, Entry::Item { text, .. } if text.contains("main process has exited"))));
        }
    }

    #[test]
    fn rules_submenu_reflects_the_saved_rule_and_the_engine_state() {
        let mut env = env();
        env.rule = Some(Actions { efficiency: Some(true), ..Default::default() });
        let es = build(&target(Origin::Processes, false, 1), &env);
        assert!(matches!(find(&es, ID_R_EFF), Some(Entry::Item { checked: true, .. })));
        assert!(enabled(&es, ID_R_REMOVE) && enabled(&es, ID_R_SHOW));
        assert!(es.iter().any(|e| matches!(e, Entry::Sub { text, .. } if text == "Rules for a.exe  ●")));
        env.rules = false;
        let es = build(&target(Origin::Top, false, 1), &env);
        assert!(find(&es, ID_R_EFF).is_none() && !enabled(&es, ID_R_VIEW));
    }

    #[test]
    fn cpu_items_follow_the_pc_and_the_representative() {
        let mut env = env();
        env.cpu = Err(2);
        let es = build(&target(Origin::Top, false, 1), &env);
        assert!(!enabled(&es, ID_CPU_AFF) && find(&es, ID_CPU_SETS).is_none());
        assert!(text(&es, ID_CPU_AFF).contains("2 processor groups"));
        env.cpu = Ok(());
        let mut t = target(Origin::Top, false, 1);
        t.rep.as_mut().unwrap().pid = 4;
        assert!(!enabled(&build(&t, &env), ID_CPU_AFF), "the System process is never pinned");
        let mut t = target(Origin::Top, false, 1);
        t.rep.as_mut().unwrap().create = 0;
        assert!(!enabled(&build(&t, &env), ID_CPU_SETS), "no create time, no identity");
    }

    #[test]
    fn disallow_item_names_the_exe_scope_and_fails_closed() {
        // clearly scoped to every copy of the exe, in Processes and Top alike
        for origin in [Origin::Processes, Origin::Top] {
            let es = build(&target(origin, true, 4), &env());
            assert_eq!(text(&es, ID_DISALLOW), "Disallow running… (every a.exe)");
            assert!(enabled(&es, ID_DISALLOW));
        }
        // refused program: shown, explained, never clickable
        let mut e = env();
        e.disallow = Err("essential Windows program, never ended".into());
        let es = build(&target(Origin::Top, false, 1), &e);
        assert!(!enabled(&es, ID_DISALLOW));
        assert!(text(&es, ID_DISALLOW).contains("never ended"));
        // no create time = no identity; no path = no image; exited representative: all off
        let mut t = target(Origin::Processes, false, 1);
        t.rep.as_mut().unwrap().create = 0;
        assert!(!enabled(&build(&t, &env()), ID_DISALLOW));
        let mut t = target(Origin::Processes, false, 1);
        t.rep.as_mut().unwrap().path = None;
        assert!(!enabled(&build(&t, &env()), ID_DISALLOW));
        let mut t = target(Origin::Processes, true, 3);
        t.rep = None;
        assert!(!enabled(&build(&t, &env()), ID_DISALLOW));
        // no rules engine: not offered at all
        let mut e = env();
        e.rules = false;
        assert!(find(&build(&target(Origin::Processes, false, 1), &e), ID_DISALLOW).is_none());
    }

    #[test]
    fn show_in_processes_only_when_there_is_somewhere_to_go() {
        let mut t = target(Origin::Top, false, 1);
        t.show = None;
        assert!(!enabled(&build(&t, &env()), ID_SHOW));
    }
}
