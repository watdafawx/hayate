//! Rule model + plain-text format (used for rules.ini and the rule clipboard).
//!
//! ```text
//! [settings]
//! ccd_presets_enabled = false
//!
//! [rule]
//! pattern = game.exe            ; exe name, full path, or glob
//! enabled = true
//! priority = high               ; idle|below|normal|above|high|realtime
//! affinity = 0-15               ; cpu list
//! cpusets = 0-15
//! io = low                      ; verylow|low|normal
//! memory = 5                    ; 1..5
//! efficiency = off              ; on|off
//! boost = on                    ; dynamic priority boost on|off
//! power = 381b4222-f694-41f0-9685-ff5bb260df2e
//! awake = system                ; system|display
//! disallow = true               ; end matching processes after they start (name / exact path only)
//! hits = 12
//! last = 1731650000 1234 ok - priority high
//! ```

use windows::core::GUID;

use super::glob;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
    Realtime,
}

impl Priority {
    pub const ALL: [Priority; 6] = [Priority::Idle, Priority::BelowNormal, Priority::Normal, Priority::AboveNormal, Priority::High, Priority::Realtime];
    pub fn key(self) -> &'static str {
        match self {
            Priority::Idle => "idle",
            Priority::BelowNormal => "below",
            Priority::Normal => "normal",
            Priority::AboveNormal => "above",
            Priority::High => "high",
            Priority::Realtime => "realtime",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Priority::Idle => "Idle",
            Priority::BelowNormal => "Below normal",
            Priority::Normal => "Normal",
            Priority::AboveNormal => "Above normal",
            Priority::High => "High",
            Priority::Realtime => "Realtime",
        }
    }
    fn parse(s: &str) -> Option<Priority> {
        Priority::ALL.into_iter().find(|p| p.key() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoPriority {
    VeryLow,
    Low,
    Normal,
}

impl IoPriority {
    pub fn key(self) -> &'static str {
        match self {
            IoPriority::VeryLow => "verylow",
            IoPriority::Low => "low",
            IoPriority::Normal => "normal",
        }
    }
    pub fn hint(self) -> u32 {
        match self {
            IoPriority::VeryLow => 0,
            IoPriority::Low => 1,
            IoPriority::Normal => 2,
        }
    }
    fn parse(s: &str) -> Option<IoPriority> {
        [IoPriority::VeryLow, IoPriority::Low, IoPriority::Normal].into_iter().find(|p| p.key() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Awake {
    /// keep system awake
    System,
    /// keep system and display awake
    Display,
}

/// What to do to a matching process. `None` = leave alone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Actions {
    pub priority: Option<Priority>,
    /// hard affinity mask (inherited by children)
    pub affinity: Option<u64>,
    /// soft CPU Sets mask (works on protected processes)
    pub cpu_sets: Option<u64>,
    pub io: Option<IoPriority>,
    /// memory page priority 1..=5
    pub memory: Option<u8>,
    pub efficiency: Option<bool>,
    /// dynamic priority boost on/off
    pub boost: Option<bool>,
    /// power plan while running, restored when the last match exits
    pub power_plan: Option<GUID>,
    pub keep_awake: Option<Awake>,
    /// "Disallow running": Blink ends a matching process after it starts. NOT a
    /// launch block. Honoured only for exe-name and exact-path rules (never globs)
    /// and only while the rule is enabled. When set, no other action is applied to
    /// the matched process: the block wins. Default off; absent from older files.
    pub disallow: bool,
}

impl Actions {
    pub fn is_empty(&self) -> bool {
        *self == Actions::default()
    }
    /// Short human summary, e.g. "High · CPU sets 0-15 · I/O low".
    pub fn summary(&self) -> String {
        let mut v: Vec<String> = Vec::new();
        if self.disallow {
            v.push("Disallow running".into());
        }
        if let Some(p) = self.priority {
            v.push(p.label().into());
        }
        if let Some(m) = self.affinity {
            v.push(format!("Affinity {}", format_cpus(m)));
        }
        if let Some(m) = self.cpu_sets {
            v.push(format!("CPU sets {}", format_cpus(m)));
        }
        if let Some(i) = self.io {
            v.push(format!("I/O {}", i.key()));
        }
        if let Some(m) = self.memory {
            v.push(format!("Memory {}", m));
        }
        if let Some(e) = self.efficiency {
            v.push(if e { "Efficiency on".into() } else { "Efficiency off".into() });
        }
        if let Some(b) = self.boost {
            v.push(if b { "Boost on".into() } else { "Boost off".into() });
        }
        if self.power_plan.is_some() {
            v.push("Power plan".into());
        }
        match self.keep_awake {
            Some(Awake::System) => v.push("Prevent sleep".into()),
            Some(Awake::Display) => v.push("Prevent sleep + display".into()),
            None => {}
        }
        v.join(" · ")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LastResult {
    /// unix seconds
    pub time: u64,
    pub pid: u32,
    pub ok: bool,
    /// target was a protected process; some actions were refused
    pub protected: bool,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchKind {
    Name,
    Path,
    Glob,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pub id: u32,
    pub enabled: bool,
    /// `game.exe`, `C:\Games\game.exe`, or a glob like `X:\Games\*\*.exe`
    pub pattern: String,
    pub actions: Actions,
    pub hits: u64,
    pub last: Option<LastResult>,
}

impl Rule {
    pub fn new(pattern: &str, actions: Actions) -> Rule {
        Rule { id: 0, enabled: true, pattern: pattern.trim().to_string(), actions, hits: 0, last: None }
    }
    pub fn kind(&self) -> MatchKind {
        kind_of(&self.pattern)
    }
    /// An enabled Disallow rule of a kind that may end processes (name or exact path).
    pub fn blocks(&self) -> bool {
        self.enabled && self.actions.disallow && self.kind() != MatchKind::Glob
    }
    /// `path` is the Win32 image path; `name` its file name.
    pub fn matches(&self, name: &str, path: &str) -> bool {
        match self.kind() {
            MatchKind::Name => self.pattern.eq_ignore_ascii_case(name),
            MatchKind::Path => self.pattern.eq_ignore_ascii_case(path),
            MatchKind::Glob => {
                if self.pattern.contains(['\\', '/']) {
                    glob::matches(&self.pattern, path)
                } else {
                    glob::matches(&self.pattern, name)
                }
            }
        }
    }
}

pub fn kind_of(pattern: &str) -> MatchKind {
    if glob::is_glob(pattern) {
        MatchKind::Glob
    } else if pattern.contains(['\\', '/']) {
        MatchKind::Path
    } else {
        MatchKind::Name
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Offer CCD0 (V-Cache) / CCD1 presets in rule pickers. Off by default:
    /// Blink never applies CPU sets / affinity unless a rule says so.
    pub ccd_presets_enabled: bool,
    /// ProBalance-like Restraint (v2, not implemented yet). Off.
    pub restraint_enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { ccd_presets_enabled: false, restraint_enabled: false }
    }
}

// ---- cpu lists -------------------------------------------------------------

/// `0-15,18` → mask.
pub fn parse_cpus(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x") {
        return u64::from_str_radix(h, 16).ok().filter(|m| *m != 0);
    }
    let mut m = 0u64;
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?),
            None => {
                let a = part.parse::<u32>().ok()?;
                (a, a)
            }
        };
        if a > b || b > 63 {
            return None;
        }
        for i in a..=b {
            m |= 1 << i;
        }
    }
    (m != 0).then_some(m)
}

pub fn format_cpus(m: u64) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < 64 {
        if m & (1 << i) != 0 {
            let start = i;
            while i + 1 < 64 && m & (1 << (i + 1)) != 0 {
                i += 1;
            }
            out.push(if start == i { format!("{}", i) } else { format!("{}-{}", start, i) });
        }
        i += 1;
    }
    out.join(",")
}

// ---- guid ------------------------------------------------------------------

pub fn format_guid(g: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1, g.data2, g.data3, g.data4[0], g.data4[1], g.data4[2], g.data4[3], g.data4[4], g.data4[5], g.data4[6], g.data4[7]
    )
}

pub fn parse_guid(s: &str) -> Option<GUID> {
    let h: String = s.trim().trim_matches(['{', '}']).chars().filter(|c| *c != '-').collect();
    if h.len() != 32 {
        return None;
    }
    u128::from_str_radix(&h, 16).ok().map(GUID::from_u128)
}

// ---- text format -----------------------------------------------------------

fn onoff(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "on" | "true" | "yes" | "1" => Some(true),
        "off" | "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

pub fn write_rule(out: &mut String, r: &Rule, with_stats: bool) {
    use std::fmt::Write;
    let a = &r.actions;
    let _ = writeln!(out, "[rule]");
    let _ = writeln!(out, "pattern = {}", one_line(&r.pattern));
    let _ = writeln!(out, "enabled = {}", r.enabled);
    if let Some(p) = a.priority {
        let _ = writeln!(out, "priority = {}", p.key());
    }
    if let Some(m) = a.affinity {
        let _ = writeln!(out, "affinity = {}", format_cpus(m));
    }
    if let Some(m) = a.cpu_sets {
        let _ = writeln!(out, "cpusets = {}", format_cpus(m));
    }
    if let Some(i) = a.io {
        let _ = writeln!(out, "io = {}", i.key());
    }
    if let Some(m) = a.memory {
        let _ = writeln!(out, "memory = {}", m);
    }
    if let Some(e) = a.efficiency {
        let _ = writeln!(out, "efficiency = {}", onoff(e));
    }
    if let Some(b) = a.boost {
        let _ = writeln!(out, "boost = {}", onoff(b));
    }
    if let Some(g) = &a.power_plan {
        let _ = writeln!(out, "power = {}", format_guid(g));
    }
    match a.keep_awake {
        Some(Awake::System) => {
            let _ = writeln!(out, "awake = system");
        }
        Some(Awake::Display) => {
            let _ = writeln!(out, "awake = display");
        }
        None => {}
    }
    if a.disallow {
        let _ = writeln!(out, "disallow = true");
    }
    if with_stats {
        if r.hits > 0 {
            let _ = writeln!(out, "hits = {}", r.hits);
        }
        if let Some(l) = &r.last {
            let _ = writeln!(out, "last = {} {} {} {} {}", l.time, l.pid, if l.ok { "ok" } else { "fail" }, if l.protected { "p" } else { "-" }, one_line(&l.text));
        }
    }
    out.push('\n');
}

pub fn write_all(rules: &[Rule], settings: &Settings) -> String {
    let mut s = String::from("; Hayate rules. Edit while Hayate is closed, or use the Rules view.\n\n[settings]\n");
    s.push_str(&format!("ccd_presets_enabled = {}\nrestraint_enabled = {}\n\n", settings.ccd_presets_enabled, settings.restraint_enabled));
    for r in rules {
        write_rule(&mut s, r, true);
    }
    s
}

/// Parse rules + settings. Unknown keys and bad values are skipped (listed in
/// the returned warnings) so a hand-edit typo never loses the whole file.
pub fn parse(text: &str) -> (Vec<Rule>, Settings, Vec<String>) {
    let mut rules = Vec::new();
    let mut settings = Settings::default();
    let mut warn = Vec::new();
    enum Sec {
        None,
        Settings,
        Rule,
    }
    let mut sec = Sec::None;
    let mut cur: Option<Rule> = None;
    let flush = |cur: &mut Option<Rule>, rules: &mut Vec<Rule>, warn: &mut Vec<String>| {
        if let Some(r) = cur.take() {
            let mut r = r;
            if r.pattern.is_empty() {
                warn.push("rule without pattern skipped".into());
            } else {
                // Disallow ends processes: a wildcard could reach far more than anyone meant
                if r.actions.disallow && r.kind() == MatchKind::Glob {
                    r.actions.disallow = false;
                    warn.push(format!("rule '{}': disallow ignored (only an exe name or an exact path may be disallowed, not a pattern with * or ?)", r.pattern));
                }
                rules.push(r);
            }
        }
    };
    for (ln, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            flush(&mut cur, &mut rules, &mut warn);
            match line {
                "[settings]" => sec = Sec::Settings,
                "[rule]" => {
                    sec = Sec::Rule;
                    cur = Some(Rule::new("", Actions::default()));
                }
                _ => {
                    warn.push(format!("line {}: unknown section {}", ln + 1, line));
                    sec = Sec::None;
                }
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            warn.push(format!("line {}: expected key = value", ln + 1));
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        // strip trailing "; comment" except in pattern/last (paths may hold ';')
        let vv = if k == "pattern" || k == "last" { v } else { v.split(" ;").next().unwrap_or(v).trim() };
        let lv = vv.to_ascii_lowercase();
        let mut bad = false;
        match sec {
            Sec::Settings => match k {
                "ccd_presets_enabled" => settings.ccd_presets_enabled = parse_bool(&lv).unwrap_or_else(|| { bad = true; false }),
                "restraint_enabled" => settings.restraint_enabled = parse_bool(&lv).unwrap_or_else(|| { bad = true; false }),
                _ => bad = true,
            },
            Sec::Rule => {
                let r = cur.as_mut().unwrap();
                let a = &mut r.actions;
                match k {
                    "pattern" => r.pattern = vv.to_string(),
                    "enabled" => r.enabled = parse_bool(&lv).unwrap_or_else(|| { bad = true; true }),
                    "priority" => a.priority = Priority::parse(&lv).or_else(|| { bad = true; None }),
                    "affinity" => a.affinity = parse_cpus(&lv).or_else(|| { bad = true; None }),
                    "cpusets" => a.cpu_sets = parse_cpus(&lv).or_else(|| { bad = true; None }),
                    "io" => a.io = IoPriority::parse(&lv).or_else(|| { bad = true; None }),
                    "memory" => a.memory = lv.parse::<u8>().ok().filter(|m| (1..=5).contains(m)).or_else(|| { bad = true; None }),
                    "efficiency" => a.efficiency = parse_bool(&lv).or_else(|| { bad = true; None }),
                    "boost" => a.boost = parse_bool(&lv).or_else(|| { bad = true; None }),
                    "disallow" => a.disallow = parse_bool(&lv).unwrap_or_else(|| { bad = true; false }),
                    "power" => a.power_plan = parse_guid(&lv).or_else(|| { bad = true; None }),
                    "awake" => {
                        a.keep_awake = match lv.as_str() {
                            "system" => Some(Awake::System),
                            "display" => Some(Awake::Display),
                            _ => {
                                bad = true;
                                None
                            }
                        }
                    }
                    "hits" => r.hits = lv.parse().unwrap_or(0),
                    "last" => {
                        let mut it = vv.splitn(5, ' ');
                        let t = it.next().and_then(|x| x.parse().ok());
                        let pid = it.next().and_then(|x| x.parse().ok());
                        let ok = it.next().map(|x| x == "ok");
                        let prot = it.next().map(|x| x == "p");
                        let text = it.next().unwrap_or("").to_string();
                        if let (Some(time), Some(pid), Some(ok), Some(protected)) = (t, pid, ok, prot) {
                            r.last = Some(LastResult { time, pid, ok, protected, text });
                        }
                    }
                    _ => bad = true,
                }
            }
            Sec::None => bad = true,
        }
        if bad {
            warn.push(format!("line {}: bad or unknown '{}'", ln + 1, line));
        }
    }
    flush(&mut cur, &mut rules, &mut warn);
    (rules, settings, warn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpus() {
        assert_eq!(parse_cpus("0-15"), Some(0xFFFF));
        assert_eq!(parse_cpus("16-31"), Some(0xFFFF_0000));
        assert_eq!(parse_cpus("0,2,4-5"), Some(0b110101));
        assert_eq!(parse_cpus("0x3"), Some(3));
        assert_eq!(parse_cpus("70"), None);
        // an empty mask is never a CPU selection, in any spelling
        assert_eq!(parse_cpus("0x0"), None);
        assert_eq!(parse_cpus("0x0000"), None);
        assert_eq!(parse_cpus("63"), Some(1 << 63));
        assert_eq!(parse_cpus("64"), None);
        assert_eq!(parse_cpus("5-3"), None);
        assert_eq!(parse_cpus(""), None);
        assert_eq!(format_cpus(0b110101), "0,2,4-5");
        assert_eq!(format_cpus(0xFFFF_0000), "16-31");
    }

    #[test]
    fn guid_roundtrip() {
        let g = parse_guid("{381b4222-f694-41f0-9685-ff5bb260df2e}").unwrap();
        assert_eq!(format_guid(&g), "381b4222-f694-41f0-9685-ff5bb260df2e");
    }

    #[test]
    fn kinds_and_matching() {
        let r = Rule::new("Game.exe", Actions::default());
        assert_eq!(r.kind(), MatchKind::Name);
        assert!(r.matches("game.EXE", "C:\\x\\game.exe"));
        let r = Rule::new("C:\\Games\\g.exe", Actions::default());
        assert_eq!(r.kind(), MatchKind::Path);
        assert!(r.matches("g.exe", "c:\\games\\G.exe"));
        assert!(!r.matches("g.exe", "D:\\games\\g.exe"));
        let r = Rule::new("X:\\Games\\*\\*.exe", Actions::default());
        assert_eq!(r.kind(), MatchKind::Glob);
        assert!(r.matches("a.exe", "X:\\Games\\A\\a.exe"));
        let r = Rule::new("rust*.exe", Actions::default());
        assert!(r.matches("rustc.exe", "C:\\t\\rustc.exe"));
    }

    #[test]
    fn text_roundtrip() {
        let mut r = Rule::new(
            "C:\\Games\\My Game\\game.exe",
            Actions {
                priority: Some(Priority::High),
                affinity: None,
                cpu_sets: Some(0xFFFF),
                io: Some(IoPriority::Low),
                memory: Some(5),
                efficiency: Some(false),
                boost: Some(true),
                power_plan: parse_guid("8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c"),
                keep_awake: Some(Awake::Display),
                disallow: true,
            },
        );
        r.hits = 7;
        r.last = Some(LastResult { time: 1_731_650_000, pid: 1234, ok: false, protected: true, text: "access denied (protected): priority".into() });
        let r2 = Rule { enabled: false, ..Rule::new("discord.exe", Actions { efficiency: Some(true), ..Default::default() }) };
        let s = Settings { ccd_presets_enabled: true, restraint_enabled: false };
        let text = write_all(&[r.clone(), r2.clone()], &s);
        let (rules, s2, warn) = parse(&text);
        assert!(warn.is_empty(), "{:?}", warn);
        assert_eq!(s2, s);
        assert_eq!(rules, vec![r, r2]);
    }

    #[test]
    fn disallow_is_off_by_default_and_old_files_never_block() {
        let old = "[settings]\nccd_presets_enabled = false\n\n[rule]\npattern = game.exe\nenabled = true\npriority = high\n\n[rule]\npattern = C:\\Games\\g.exe\nenabled = true\naffinity = 0-3\n";
        let (rules, _, warn) = parse(old);
        assert!(warn.is_empty(), "{warn:?}");
        assert!(rules.iter().all(|r| !r.actions.disallow && !r.blocks()));
        assert!(!Actions::default().disallow);
        assert!(Rule::new("a.exe", Actions::default()).actions.is_empty());
        // writing an ordinary rule never mentions it
        assert!(!write_all(&rules, &Settings::default()).contains("disallow"));
    }

    #[test]
    fn disallow_only_rule_parses_and_roundtrips() {
        let d = Actions { disallow: true, ..Default::default() };
        assert!(!d.is_empty(), "a Disallow-only rule is a real rule, not an empty one");
        let r = Rule::new("zz_disallow.exe", d.clone());
        let p = Rule { enabled: false, ..Rule::new(r"C:\Games\Game One\g.exe", d) };
        let text = write_all(&[r.clone(), p.clone()], &Settings::default());
        assert_eq!(text.matches("disallow = true").count(), 2, "{text}");
        let (back, _, warn) = parse(&text);
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(back, vec![r.clone(), p.clone()]);
        assert!(r.blocks() && !p.blocks(), "a disabled rule never blocks");
        assert_eq!(r.actions.summary(), "Disallow running");
        // the clipboard form carries it too (and `paste` imports it OFF, see rules::tests)
        let mut one = String::new();
        write_rule(&mut one, &r, false);
        assert!(one.contains("disallow = true"));
    }

    #[test]
    fn disallow_with_other_settings_keeps_both_and_says_so() {
        let a = Actions { disallow: true, priority: Some(Priority::High), ..Default::default() };
        assert_eq!(a.summary(), "Disallow running · High");
        let (rules, _, _) = parse(&write_all(&[Rule::new("a.exe", a.clone())], &Settings::default()));
        assert_eq!(rules[0].actions, a);
    }

    #[test]
    fn a_wildcard_disallow_is_refused_at_parse_and_a_bad_value_never_blocks() {
        let (rules, _, warn) = parse("[rule]\npattern = *.exe\ndisallow = true\npriority = idle\n[rule]\npattern = X:\\Games\\*\\g.exe\ndisallow = true\n[rule]\npattern = a.exe\ndisallow = maybe\n[rule]\npattern = b.exe\ndisallow = off\n");
        assert_eq!(rules.len(), 4);
        assert!(rules.iter().all(|r| !r.actions.disallow && !r.blocks()), "{rules:?}");
        assert_eq!(rules[0].actions.priority, Some(Priority::Idle), "the rest of the rule survives");
        assert_eq!(warn.iter().filter(|w| w.contains("disallow ignored")).count(), 2, "{warn:?}");
        assert!(warn.iter().any(|w| w.contains("bad or unknown 'disallow = maybe'")), "{warn:?}");
    }

    #[test]
    fn blocks_needs_enabled_and_a_name_or_path() {
        let d = Actions { disallow: true, ..Default::default() };
        assert!(Rule::new("a.exe", d.clone()).blocks());
        assert!(Rule::new(r"C:\x\a.exe", d.clone()).blocks());
        assert!(!Rule { enabled: false, ..Rule::new("a.exe", d.clone()) }.blocks());
        // not constructible through the parser, but the type itself still refuses a glob
        assert!(!Rule::new("a*.exe", d).blocks());
    }

    #[test]
    fn parse_tolerates_junk() {
        let (rules, s, warn) = parse("[settings]\nbogus = 1\n[rule]\npattern = a.exe\npriority = turbo\nio = low ; comment\n[rule]\nenabled = true\n");
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].actions.io, Some(IoPriority::Low));
        assert_eq!(rules[0].actions.priority, None);
        assert!(!s.ccd_presets_enabled);
        assert_eq!(warn.len(), 3);
    }
}
