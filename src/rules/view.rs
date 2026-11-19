//! Read-only view of the rules for the Processes tab: which saved rules match a
//! process image, which one the engine would apply, and why the others do not.
//!
//! Matching is the engine's own (`Rule::matches`, same precedence as
//! `Inner::find`: exact path, then exe name, then glob; within one kind the
//! earlier rule wins; disabled rules never apply). Nothing here changes a rule
//! or touches a process: it only reads a copy of the rule list.
//!
//! A rule's actions are what is CONFIGURED. Whether a process still has those
//! settings now is not known here (Windows or another tool may have changed
//! them since), and `last` is the last application to ANY process of the rule.

use super::model::{MatchKind, Rule};
use crate::osutil;

/// How one matching rule stands for one process image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// the rule the engine applies to this process
    Effective,
    /// enabled and matching, but the rule `by` (id) wins for this process
    Shadowed { by: u32 },
    /// matching, but switched off: applies to nobody until enabled
    Disabled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Matching {
    pub rule: Rule,
    pub standing: Standing,
}

/// What one process shows in the Processes tab: a compact summary of `matching`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mark {
    /// the image path is not readable: rules cannot be matched
    Unknown,
    /// no rule, enabled or not, matches
    NoRule,
    /// the enabled rule (id) the engine applies
    Rule(u32),
    /// only a disabled rule (id of the first one) matches: nothing is applied
    Off(u32),
}

/// A copy of the rule list: no lock is held while it is used.
#[derive(Clone, Debug, Default)]
pub struct RuleView {
    rules: Vec<Rule>,
}

/// The engine's choice among `rules` for this image: exact path > exe name > glob,
/// first enabled rule within each kind.
pub fn pick(rules: &[Rule], name: &str, path: &str) -> Option<usize> {
    [MatchKind::Path, MatchKind::Name, MatchKind::Glob]
        .into_iter()
        .find_map(|k| rules.iter().position(|r| r.enabled && r.kind() == k && r.matches(name, path)))
}

impl RuleView {
    pub fn new(rules: Vec<Rule>) -> RuleView {
        RuleView { rules }
    }

    /// Every rule that matches this image: the effective one first, then the
    /// shadowed ones (precedence order), then the disabled ones.
    pub fn matching(&self, path: &str) -> Vec<Matching> {
        if path.is_empty() {
            return Vec::new();
        }
        let name = osutil::file_name(path);
        let eff = pick(&self.rules, name, path);
        let mut out: Vec<(u8, usize, Matching)> = Vec::new();
        for (i, r) in self.rules.iter().enumerate() {
            if !r.matches(name, path) {
                continue;
            }
            let (standing, tier) = match (Some(i) == eff, r.enabled, eff) {
                (true, ..) => (Standing::Effective, 0),
                (_, true, Some(e)) => (Standing::Shadowed { by: self.rules[e].id }, 1),
                (_, true, None) => continue, // unreachable: an enabled match always yields an effective rule
                (_, false, _) => (Standing::Disabled, 2),
            };
            // shadowed rules by the engine's own precedence (path, name, glob), then file order
            let kind = match r.kind() {
                MatchKind::Path => 0,
                MatchKind::Name => 1,
                MatchKind::Glob => 2,
            };
            out.push((tier * 3 + kind, i, Matching { rule: r.clone(), standing }));
        }
        out.sort_by_key(|(t, i, _)| (*t, *i));
        out.into_iter().map(|(_, _, m)| m).collect()
    }

    /// Compact state of a process image; `None` path = unreadable.
    pub fn mark(&self, path: Option<&str>) -> Mark {
        let Some(path) = path.filter(|p| !p.is_empty()) else { return Mark::Unknown };
        let name = osutil::file_name(path);
        if let Some(i) = pick(&self.rules, name, path) {
            return Mark::Rule(self.rules[i].id);
        }
        match self.rules.iter().find(|r| !r.enabled && r.matches(name, path)) {
            Some(r) => Mark::Off(r.id),
            None => Mark::NoRule,
        }
    }

    pub fn get(&self, id: u32) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }
}

/// What a group of processes shows: one common state, or mixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupMark {
    /// every member has this same state
    Same(Mark),
    /// at least two members differ and at least one has a rule (enabled or not)
    Mixed,
    /// members differ only between "no rule" and "unreadable": nothing to show
    Plain,
}

/// Combine member marks. Never claims the root's rule for the whole group.
pub fn group_mark(marks: impl IntoIterator<Item = Mark>) -> GroupMark {
    let mut it = marks.into_iter();
    let Some(first) = it.next() else { return GroupMark::Plain };
    let mut all_same = true;
    let mut any_rule = matches!(first, Mark::Rule(_) | Mark::Off(_));
    for m in it {
        all_same &= m == first;
        any_rule |= matches!(m, Mark::Rule(_) | Mark::Off(_));
    }
    match (all_same, any_rule) {
        (true, _) => GroupMark::Same(first),
        (false, true) => GroupMark::Mixed,
        (false, false) => GroupMark::Plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::model::Actions;
    use crate::rules::Priority;

    fn rule(id: u32, pattern: &str, enabled: bool) -> Rule {
        let mut r = Rule::new(pattern, Actions { priority: Some(Priority::High), ..Default::default() });
        r.id = id;
        r.enabled = enabled;
        r
    }

    #[test]
    fn precedence_is_path_then_name_then_glob_and_earlier_wins() {
        let v = RuleView::new(vec![
            rule(1, r"X:\Games\*", true),
            rule(2, "game.exe", true),
            rule(3, r"X:\Games\a\game.exe", true),
            rule(4, "game.exe", true),
        ]);
        let m = v.matching(r"X:\Games\a\game.exe");
        let order: Vec<(u32, Standing)> = m.iter().map(|m| (m.rule.id, m.standing)).collect();
        assert_eq!(order, vec![(3, Standing::Effective), (2, Standing::Shadowed { by: 3 }), (4, Standing::Shadowed { by: 3 }), (1, Standing::Shadowed { by: 3 })]);
        // same name elsewhere: the name rule wins over the glob; the later same-name rule is shadowed by the earlier
        let order: Vec<(u32, Standing)> = v.matching(r"X:\Games\b\game.exe").iter().map(|m| (m.rule.id, m.standing)).collect();
        assert_eq!(order, vec![(2, Standing::Effective), (4, Standing::Shadowed { by: 2 }), (1, Standing::Shadowed { by: 2 })]);
        // only the glob
        assert_eq!(v.mark(Some(r"X:\Games\b\other.exe")), Mark::Rule(1));
        assert_eq!(v.mark(Some(r"C:\x\other.exe")), Mark::NoRule);
    }

    #[test]
    fn disabled_rules_never_win_and_are_reported_honestly() {
        let v = RuleView::new(vec![rule(1, "game.exe", false), rule(2, r"X:\Games\*", true)]);
        // the disabled name rule is not effective: the enabled glob applies
        let m = v.matching(r"X:\Games\game.exe");
        assert_eq!(m[0].standing, Standing::Effective);
        assert_eq!(m[0].rule.id, 2);
        assert_eq!(m[1].standing, Standing::Disabled);
        assert_eq!(v.mark(Some(r"X:\Games\game.exe")), Mark::Rule(2));
        // only a disabled rule matches: nothing applies, and it says so
        let v = RuleView::new(vec![rule(1, "game.exe", false)]);
        assert_eq!(v.mark(Some(r"C:\game.exe")), Mark::Off(1));
        assert_eq!(v.matching(r"C:\game.exe")[0].standing, Standing::Disabled);
        assert_eq!(v.mark(Some(r"C:\other.exe")), Mark::NoRule);
    }

    #[test]
    fn unreadable_path_is_unknown_not_unruled() {
        let v = RuleView::new(vec![rule(1, "game.exe", true)]);
        assert_eq!(v.mark(None), Mark::Unknown);
        assert_eq!(v.mark(Some("")), Mark::Unknown);
        assert!(v.matching("").is_empty());
    }

    #[test]
    fn a_group_never_claims_one_rule_for_mixed_members() {
        use Mark::*;
        assert_eq!(group_mark([Rule(1), Rule(1), Rule(1)]), GroupMark::Same(Rule(1)));
        assert_eq!(group_mark([NoRule, NoRule]), GroupMark::Same(NoRule));
        assert_eq!(group_mark([Rule(1), NoRule]), GroupMark::Mixed, "root has a rule, a child has none");
        assert_eq!(group_mark([NoRule, Rule(1)]), GroupMark::Mixed);
        assert_eq!(group_mark([Rule(1), Rule(2)]), GroupMark::Mixed);
        assert_eq!(group_mark([Rule(1), Off(2)]), GroupMark::Mixed);
        assert_eq!(group_mark([Off(2), Off(2)]), GroupMark::Same(Off(2)));
        assert_eq!(group_mark([Rule(1), Unknown]), GroupMark::Mixed, "an unreadable member is not assumed to share the rule");
        assert_eq!(group_mark([NoRule, Unknown]), GroupMark::Plain);
        assert_eq!(group_mark([] as [Mark; 0]), GroupMark::Plain);
    }
}
