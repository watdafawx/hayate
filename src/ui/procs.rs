//! Processes view: groups processes into apps / background / Windows like
//! Task Manager, each group expandable to its member processes.
//! Columns can be toggled and resized; sorting works on group totals.
//!
//! Rows are keyed by process identity (PID + create time), never by index
//! alone: values refresh every tick, the ORDER only per `order` (held while
//! the pointer is in the list, 5 s otherwise), and every click / key / menu
//! target is resolved by identity against the newest snapshot.

use super::order::{self, Slot, Stable};
use super::procmenu::{Origin, Rep, Target};
use super::*;
use crate::rules::view::GroupMark;
use crate::sample::{Proc, Snapshot};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Col {
    Name,
    Type,
    Status,
    Publisher,
    Pid,
    ProcName,
    CmdLine,
    Cpu,
    Mem,
    Disk,
    Gpu,
    GpuEngine,
    Threads,
    Handles,
    Path,
}

pub struct ColState {
    pub col: Col,
    pub label: &'static str,
    pub w: f32,
    pub on: bool,
    pub right: bool,
}

pub const ROW_H: f32 = 22.0;
const HEAD_H: f32 = 40.0;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Kind {
    App,
    Background,
    Windows,
}

pub struct Group {
    pub key: (u32, i64),
    pub name: String,
    pub kind: Kind,
    pub members: Vec<usize>,
    pub cpu: f32,
    pub mem: u64,
    pub io: f64,
    pub gpu: f32,
    /// engine of the busiest member
    pub gpu_engine: Option<std::sync::Arc<str>>,
    gpu_top: f32,
    pub threads: u32,
    pub handles: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Row {
    Header(Kind, usize),
    Group(usize),
    Child(usize, usize),
    /// an exited process whose slot is held while the order is frozen (index into `gone`)
    Gone(usize),
}

/// Stable identity of a row: (pid, create time, is app group).
pub type RowId = (u32, i64, bool);

/// A held slot of a process that exited.
#[derive(Clone, Debug, PartialEq)]
pub struct Gone {
    pub key: (u32, i64),
    pub name: String,
    pub child: bool,
    /// the process still runs but is no longer a group of its own (shown "Moved", not "Exited")
    pub alive: bool,
}

/// What the pointer/user is doing, for the order hold (see `order`).
#[derive(Clone, Copy, Debug)]
pub struct Interact {
    pub now: u64,
    /// pointer inside the list
    pub inside: bool,
    /// button held, scrollbar drag or a menu open
    pub active: bool,
}

pub struct ProcView {
    pub cols: Vec<ColState>,
    pub sort: Col,
    pub desc: bool,
    pub expanded: HashSet<(u32, i64)>,
    pub sel: Option<(u32, i64, bool)>, // (pid, create, is_group)
    /// "Show in Processes": scroll this row into view at the next paint, once, then forget it
    pub reveal: Option<RowId>,
    pub scroll: f32,
    pub hs: f32,
    pub groups: Vec<Group>,
    pub rows: Vec<Row>,
    /// identity of each row of `rows` (None: header / exited slot); filled by `rebuild`
    pub row_ids: Vec<Option<RowId>>,
    pub gone: Vec<Gone>,
    /// the snapshot `groups` / `rows` were built from (their indexes refer to it)
    pub built: Option<Arc<Snapshot>>,
    /// held order per section (App / Background / Windows) and per expanded group
    kinds: [Stable<(u32, i64)>; 3],
    kids: HashMap<(u32, i64), Stable<(u32, i64)>>,
    /// last known names, for exited slots
    names: HashMap<(u32, i64), String>,
    /// list area of the last paint
    pub list: Rect,
    /// last wheel / key interaction (order::now_ms), 0 = none
    touch_at: u64,
    /// identity under the last press: a release or double click acts only on the same row
    pub pressed: Option<RowId>,
    pub dirty: bool,
    view_h: f32,
    content_h: f32,
    resize: Option<(usize, f32, f32)>,
    sdrag: Option<(f32, f32)>,
    track: Rect,
    pending_move: i32,
    /// shown columns of the last paint as (x0, x1, index into `cols`), for hover hit-testing
    pub cols_x: Vec<(f32, f32, usize)>,
}

impl ProcView {
    pub fn new() -> Self {
        use Col::*;
        let c = |col, label, w, on, right| ColState { col, label, w, on, right };
        ProcView {
            cols: vec![
                c(Name, "Name", 260.0, true, false),
                c(Type, "Type", 90.0, false, false),
                c(Status, "Status", 70.0, true, false),
                c(Publisher, "Publisher", 150.0, false, false),
                c(Pid, "PID", 64.0, true, true),
                c(ProcName, "Process name", 140.0, false, false),
                // optional and hidden on a fresh start: the Name card and clipped-cell tips carry the full text
                c(CmdLine, "Command line", 280.0, false, false),
                c(Path, "Image path", 260.0, false, false),
                c(Cpu, "CPU", 64.0, true, true),
                c(Mem, "Memory", 86.0, true, true),
                c(Disk, "Disk I/O", 86.0, true, true),
                c(Gpu, "GPU", 58.0, true, true),
                c(GpuEngine, "GPU engine", 110.0, true, false),
                c(Threads, "Threads", 64.0, false, true),
                c(Handles, "Handles", 70.0, false, true),
            ],
            sort: Col::Cpu,
            desc: true,
            expanded: HashSet::new(),
            sel: None,
            reveal: None,
            scroll: 0.0,
            hs: 0.0,
            groups: vec![],
            rows: vec![],
            row_ids: vec![],
            gone: vec![],
            built: None,
            kinds: Default::default(),
            kids: HashMap::new(),
            names: HashMap::new(),
            list: Rect::default(),
            touch_at: 0,
            pressed: None,
            dirty: true,
            view_h: 0.0,
            content_h: 0.0,
            resize: None,
            sdrag: None,
            track: Rect::default(),
            pending_move: 0,
            cols_x: Vec::new(),
        }
    }

    pub fn toggle_col(&mut self, i: usize) {
        if let Some(c) = self.cols.get_mut(i) {
            if c.col != Col::Name {
                c.on = !c.on;
            }
        }
    }

    /// Whether a GPU column is shown (the sampler then reads per-process GPU).
    pub fn wants_gpu(&self) -> bool {
        self.cols.iter().any(|c| c.on && matches!(c.col, Col::Gpu | Col::GpuEngine))
    }

    fn on_cols(&self) -> Vec<usize> {
        (0..self.cols.len()).filter(|&i| self.cols[i].on).collect()
    }

    pub fn sort_by(&mut self, i: usize) {
        let c = self.cols[i].col;
        if self.sort == c {
            self.desc = !self.desc;
        } else {
            self.sort = c;
            self.desc = !matches!(c, Col::Name | Col::Publisher | Col::ProcName | Col::CmdLine | Col::Type | Col::Path | Col::Pid);
        }
        self.dirty = true;
    }

    pub fn begin_resize(&mut self, i: usize, x: f32) {
        self.resize = Some((i, x, self.cols[i].w));
    }
    pub fn drag_resize(&mut self, _i: usize, x: f32) {
        if let Some((i, x0, w0)) = self.resize {
            self.cols[i].w = (w0 + x - x0).max(40.0);
        }
    }
    pub fn begin_scroll(&mut self, y: f32) {
        self.sdrag = Some((y, self.scroll));
    }
    pub fn drag_scroll(&mut self, y: f32) {
        if let Some((y0, s0)) = self.sdrag {
            let ratio = self.content_h / self.track.h.max(1.0);
            self.scroll = s0 + (y - y0) * ratio;
            self.clamp();
            self.touch();
        }
    }
    pub fn scroll(&mut self, delta: f32) {
        self.scroll += delta / 120.0 * ROW_H * 3.0;
        self.clamp();
        self.touch();
    }
    pub fn hscroll(&mut self, delta: f32) {
        self.hs = (self.hs + delta / 120.0 * 60.0).max(0.0);
    }
    fn clamp(&mut self) {
        self.scroll = self.scroll.clamp(0.0, (self.content_h - self.view_h).max(0.0));
    }

    /// (row index, column index into `cols`) under a point, from the last paint's geometry.
    /// Only the visible part of the list counts: the header, the scrollbar gutter and
    /// anything above or below the list are no cell.
    pub fn cell_at(&self, x: f32, y: f32) -> Option<(usize, usize)> {
        if !self.list.contains(x, y) {
            return None;
        }
        let ri = ((y - self.list.y + self.scroll) / ROW_H).floor();
        if ri < 0.0 || ri as usize >= self.rows.len() {
            return None;
        }
        let ci = self.cols_x.iter().find(|(x0, x1, _)| x >= *x0 && x < *x1).map(|c| c.2)?;
        Some((ri as usize, ci))
    }

    /// Screen rect of cell (`ri`, `ci`) in the last paint's coordinates, whole (not cut to the list).
    pub fn cell_rect(&self, ri: usize, ci: usize) -> Option<Rect> {
        let (x0, x1, _) = *self.cols_x.iter().find(|c| c.2 == ci)?;
        Some(Rect::new(x0, self.list.y + ri as f32 * ROW_H - self.scroll, x1 - x0, ROW_H))
    }

    /// Identity of row `i` of the last paint (None: header, exited slot, or no such row).
    pub fn id_at(&self, i: usize) -> Option<RowId> {
        self.row_ids.get(i).copied().flatten()
    }

    /// Mouse press on row `i`: select what is under the pointer NOW (the rows
    /// of the last paint are exactly what the user sees) and remember it, so
    /// the release / double click only acts on the same process.
    pub fn select_row(&mut self, i: usize) {
        self.pressed = self.id_at(i);
        self.sel = self.pressed;
        self.touch();
    }

    /// Mouse press that selects nothing (chevron).
    pub fn press_row(&mut self, i: usize) {
        self.pressed = self.id_at(i);
        self.touch();
    }

    /// The row under the pointer is still the one that was pressed.
    pub fn same_press(&self, i: usize) -> bool {
        let id = self.id_at(i);
        id.is_some() && id == self.pressed
    }

    /// A wheel notch / key / click: the order holds for the next 2 s.
    pub fn touch(&mut self) {
        self.touch_at = order::now_ms();
    }

    /// Expand/collapse the group on row `i` (identity from the last paint).
    pub fn toggle_row(&mut self, i: usize) {
        if let Some((pid, ct, true)) = self.id_at(i) {
            let k = (pid, ct);
            if !self.expanded.remove(&k) {
                self.expanded.insert(k);
            }
            self.dirty = true;
        }
    }

    pub fn move_sel(&mut self, d: i32) {
        self.pending_move = d;
        self.touch();
    }

    pub fn expand_sel(&mut self, open: Option<bool>) {
        if let Some((pid, ct, true)) = self.sel {
            let k = (pid, ct);
            let now = self.expanded.contains(&k);
            let want = open.unwrap_or(!now);
            if want {
                self.expanded.insert(k);
            } else {
                self.expanded.remove(&k);
            }
            self.dirty = true;
        }
    }

    /// Processes covered by the current selection as (pid, create time, name),
    /// resolved against `latest`: only those still running with the same
    /// create time. A group is expanded from the snapshot its rows were built
    /// from (what the user saw), then every member is re-checked.
    pub fn selected_pids(&self, latest: &Snapshot) -> Vec<(u32, i64, String)> {
        let alive: HashSet<(u32, i64)> = latest.procs.iter().map(|p| (p.pid, p.create)).collect();
        match self.sel {
            Some((pid, ct, true)) => {
                let (Some(built), Some(g)) = (self.built.as_ref(), self.groups.iter().find(|g| g.key == (pid, ct))) else { return vec![] };
                g.members.iter().filter_map(|&i| built.procs.get(i)).filter(|p| alive.contains(&(p.pid, p.create))).map(|p| (p.pid, p.create, p.name.to_string())).collect()
            }
            Some((pid, ct, false)) => latest.procs.iter().filter(|p| p.pid == pid && p.create == ct).map(|p| (p.pid, p.create, p.name.to_string())).collect(),
            None => vec![],
        }
    }

    /// What a right-click on the selected row opens the menu for, fixed now: the
    /// representative (a group's root, or the one process) and the members still
    /// running. None: the row's processes are gone, so there is nothing to act on.
    pub fn menu_target(&self, latest: &Snapshot) -> Option<Target> {
        let id = self.sel?;
        let members = self.selected_pids(latest);
        if members.is_empty() {
            return None;
        }
        let rep = self.selected_proc(latest).map(|p| Rep {
            pid: p.pid,
            create: p.create,
            name: p.name.to_string(),
            path: p.meta.path.as_deref().map(String::from),
            cmd: p.meta.cmd.as_deref().map(String::from),
        });
        let label = match id {
            (pid, ct, true) => self.groups.iter().find(|g| g.key == (pid, ct)).map(|g| g.name.clone()),
            _ => rep.as_ref().map(|r| r.name.clone()),
        }
        .or_else(|| rep.as_ref().map(|r| r.name.clone()))?;
        Some(Target { origin: Origin::Processes, label, group: id.2, rep, members, show: None })
    }

    /// Select and scroll to the row for `id` (a Top list's "Show in Processes"): `expand` opens
    /// the group that holds it. The caller clears search; selection waits for a visible row.
    pub fn show(&mut self, id: RowId, expand: Option<(u32, i64)>) {
        if let Some(k) = expand {
            self.expanded.insert(k);
        }
        self.sel = None;
        self.reveal = Some(id);
        self.dirty = true;
        self.touch();
    }

    /// Order hold needs a repaint (order due to update, nothing else changed).
    pub fn due(&self, now: u64) -> bool {
        self.kinds.iter().any(|k| k.due(now)) || self.kids.values().any(|k| k.due(now))
    }

    /// The single process that represents the selection (group root or child).
    pub fn selected_proc<'a>(&self, snap: &'a Snapshot) -> Option<&'a Proc> {
        let (pid, ct, _) = self.sel?;
        snap.procs.iter().find(|p| p.pid == pid && p.create == ct)
    }

    pub fn rebuild(&mut self, snap_arc: &Arc<Snapshot>, filter: &str, ix: Interact) {
        let snap: &Snapshot = snap_arc;
        // anchor: the first fully visible real row, so an order update does not move the viewport
        let anchor = {
            let first = (self.scroll / ROW_H).ceil() as usize;
            (first..self.rows.len()).find_map(|i| self.row_ids.get(i).copied().flatten().map(|id| (id, i as f32 * ROW_H - self.scroll)))
        };
        self.groups = build_groups(snap);
        self.built = Some(snap_arc.clone());
        let q = filter.to_lowercase();
        let matches = |p: &Proc| {
            q.is_empty()
                || p.name.to_lowercase().contains(&q)
                || p.pid.to_string() == q
                || p.meta.desc.as_deref().map(|d| d.to_lowercase().contains(&q)).unwrap_or(false)
                || p.meta.publisher.as_deref().map(|d| d.to_lowercase().contains(&q)).unwrap_or(false)
                || p.meta.cmd.as_deref().map(|d| d.to_lowercase().contains(&q)).unwrap_or(false)
                || p.meta.workload.as_deref().map(|w| w.label.to_lowercase().contains(&q)).unwrap_or(false)
        };
        let sort = self.sort;
        let desc = self.desc;
        let gkey = |g: &Group| -> SortKey {
            match sort {
                Col::Cpu => SortKey::F(g.cpu as f64),
                Col::Mem => SortKey::F(g.mem as f64),
                Col::Disk => SortKey::F(g.io),
                Col::Gpu => SortKey::F(g.gpu as f64),
                Col::GpuEngine => SortKey::S(g.gpu_engine.as_deref().unwrap_or("").to_lowercase()),
                Col::Threads => SortKey::F(g.threads as f64),
                Col::Handles => SortKey::F(g.handles as f64),
                Col::Pid => SortKey::F(g.key.0 as f64),
                Col::Publisher => SortKey::S(snap.procs[g.members[0]].meta.publisher.as_deref().unwrap_or("").to_lowercase()),
                Col::ProcName => SortKey::S(snap.procs[g.members[0]].name.to_lowercase()),
                Col::CmdLine => SortKey::S(snap.procs[g.members[0]].meta.cmd.as_deref().unwrap_or("").to_lowercase()),
                Col::Path => SortKey::S(snap.procs[g.members[0]].meta.path.as_deref().unwrap_or("").to_lowercase()),
                _ => SortKey::S(g.name.to_lowercase()),
            }
        };
        let pkey = |p: &Proc| -> SortKey {
            match sort {
                Col::Cpu => SortKey::F(p.cpu as f64),
                Col::Mem => SortKey::F(p.mem as f64),
                Col::Disk => SortKey::F(p.io_bps),
                Col::Gpu => SortKey::F(p.gpu as f64),
                Col::GpuEngine => SortKey::S(p.gpu_engine.as_deref().unwrap_or("").to_lowercase()),
                Col::Threads => SortKey::F(p.threads as f64),
                Col::Handles => SortKey::F(p.handles as f64),
                Col::Pid => SortKey::F(p.pid as f64),
                _ => SortKey::S(p.name.to_lowercase()),
            }
        };
        let mut idx: Vec<usize> = (0..self.groups.len())
            .filter(|&gi| self.groups[gi].members.iter().any(|&m| matches(&snap.procs[m])))
            .collect();
        idx.sort_by(|&a, &b| {
            let o = gkey(&self.groups[a]).cmp(&gkey(&self.groups[b]));
            let o = if desc { o.reverse() } else { o };
            o.then(self.groups[a].name.cmp(&self.groups[b].name))
        });
        // explicit state: a change here applies at once and starts a new stable order
        let spec = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (sort as u8, desc, &q).hash(&mut h);
            h.finish()
        };
        let mut by_key: HashMap<(u32, i64), usize> = HashMap::with_capacity(self.groups.len());
        let mut gi_of: Vec<usize> = vec![0; snap.procs.len()];
        for (gi, g) in self.groups.iter().enumerate() {
            by_key.insert(g.key, gi);
            for &m in &g.members {
                gi_of[m] = gi;
            }
        }
        let by_proc: HashMap<(u32, i64), usize> = snap.procs.iter().enumerate().map(|(i, p)| ((p.pid, p.create), i)).collect();
        let touch = self.touch_at;
        let hold = |s: &mut Stable<(u32, i64)>| {
            s.pointer(ix.now, ix.inside);
            s.active(ix.now, ix.active);
            if touch != 0 {
                s.touch(touch);
            }
        };

        // Pass 1: held order per section. A group that is alive but now belongs to
        // another section keeps its held slot here (it is the same process, shown with
        // live values) until the order is applied; it is never shown twice.
        let mut reordered = false;
        let mut explicit = false;
        // Only when the whole list is blank (the first paint of a hidden window has no rows):
        // nothing is under the pointer, so adopt the live order. A section that is empty
        // beside shown ones keeps waiting, a new header must not push rows down.
        if self.rows.is_empty() {
            self.kinds.iter_mut().for_each(Stable::restart_if_empty);
        }
        let mut sections: Vec<(Kind, Vec<Slot<(u32, i64), usize>>)> = Vec::new();
        for (ki, kind) in [Kind::App, Kind::Background, Kind::Windows].into_iter().enumerate() {
            let live: Vec<(u32, i64)> = idx.iter().copied().filter(|&g| self.groups[g].kind == kind).map(|g| self.groups[g].key).collect();
            let st = &mut self.kinds[ki];
            hold(st);
            let a = st.arrange(ix.now, spec, live, |k| by_key.get(k).copied());
            reordered |= a.applied;
            explicit |= a.explicit;
            sections.push((kind, a.slots));
        }
        let placed: HashSet<(u32, i64)> = sections
            .iter()
            .flat_map(|(kind, slots)| slots.iter().filter_map(move |s| if let Slot::Live(gi) = s { Some((*kind, *gi)) } else { None }))
            .filter(|(kind, gi)| self.groups[*gi].kind == *kind)
            .map(|(_, gi)| self.groups[gi].key)
            .collect();
        for (kind, slots) in &mut sections {
            slots.retain(|s| match s {
                Slot::Live(gi) => self.groups[*gi].kind == *kind || !placed.contains(&self.groups[*gi].key),
                Slot::Gone(_) => true,
            });
        }

        // Pass 2: rows.
        let mut out = Out { rows: Vec::new(), ids: Vec::new(), gone: Vec::new() };
        let mut used_kids: HashSet<(u32, i64)> = HashSet::new();
        let mut names: HashMap<(u32, i64), String> = HashMap::new();
        for (kind, slots) in sections {
            if slots.is_empty() {
                continue;
            }
            let n = slots.iter().filter(|s| matches!(s, Slot::Live(_))).count();
            out.rows.push(Row::Header(kind, n));
            out.ids.push(None);
            for slot in slots {
                match slot {
                    Slot::Gone(k) => {
                        // the root left (or stopped being a group root) while the order is held
                        let alive = by_proc.get(&k).copied();
                        let name = match alive {
                            Some(pi) => snap.procs[pi].name.to_string(),
                            None => self.names.get(&k).cloned().unwrap_or_else(|| "process".into()),
                        };
                        names.insert(k, name.clone());
                        out.rows.push(Row::Gone(out.gone.len()));
                        out.ids.push(None);
                        out.gone.push(Gone { key: k, name, child: false, alive: alive.is_some() });
                        // the block the user had open stays, with its surviving processes, until thaw
                        if let Some(ks) = self.kids.get_mut(&k) {
                            hold(ks);
                            let a = ks.arrange(ix.now, spec, Vec::new(), |c| by_proc.get(c).copied());
                            reordered |= a.applied;
                            explicit |= a.explicit;
                            if !a.slots.is_empty() {
                                used_kids.insert(k);
                                push_kids(&mut out, a.slots, snap, &gi_of, &self.names, &mut names);
                            }
                        }
                    }
                    Slot::Live(gi) => {
                        let g = &self.groups[gi];
                        names.insert(g.key, g.name.clone());
                        out.rows.push(Row::Group(gi));
                        out.ids.push(Some((g.key.0, g.key.1, true)));
                        let want = (self.expanded.contains(&g.key) || !q.is_empty() && g.members.len() < 40) && (self.expanded.contains(&g.key) || q.len() >= 2);
                        // a block already on screen stays while it still has a held slot to show,
                        // even when the live group has shrunk to one process
                        let held = self.kids.get(&g.key).is_some_and(|k| k.keys().len() > 1);
                        if want && (g.members.len() > 1 || held) {
                            let mut kids: Vec<usize> = g.members.clone();
                            kids.sort_by(|&a, &b| {
                                let o = pkey(&snap.procs[a]).cmp(&pkey(&snap.procs[b]));
                                if desc { o.reverse() } else { o }
                            });
                            let live: Vec<(u32, i64)> = kids.iter().map(|&k| (snap.procs[k].pid, snap.procs[k].create)).collect();
                            let ks = self.kids.entry(g.key).or_default();
                            hold(ks);
                            let a = ks.arrange(ix.now, spec, live, |k| by_proc.get(k).copied());
                            reordered |= a.applied;
                            explicit |= a.explicit;
                            if g.members.len() > 1 || a.slots.len() > 1 {
                                used_kids.insert(g.key);
                                push_kids(&mut out, a.slots, snap, &gi_of, &self.names, &mut names);
                            }
                        }
                    }
                }
            }
        }
        self.kids.retain(|k, _| used_kids.contains(k));
        self.rows = out.rows;
        self.row_ids = out.ids;
        self.gone = out.gone;
        self.names = names;
        self.content_h = self.rows.len() as f32 * ROW_H;
        // an automatic reorder keeps the same process at the same place on screen
        if reordered && !explicit {
            if let Some((id, off)) = anchor {
                if let Some(i) = self.row_ids.iter().position(|r| *r == Some(id)) {
                    self.scroll = i as f32 * ROW_H - off;
                }
            }
        }
        self.clamp();
        self.dirty = false;
    }
}

/// `roots` plus every descendant, from one snapshot. A parent is expanded only
/// when the snapshot holds EXACTLY that process (pid and create time): `ppid`
/// names a PID, and a PID that was reused (or whose process left) owns no one's
/// children, so the replacement root's children are never adopted. Each target is
/// still re-verified against the OS handle just before it is ended.
pub fn with_descendants(procs: &[Proc], roots: Vec<(u32, i64)>) -> Vec<(u32, i64)> {
    let mut v = roots;
    let mut i = 0;
    while i < v.len() {
        let (parent, pc) = v[i];
        if procs.iter().any(|p| p.pid == parent && p.create == pc) {
            for p in procs {
                // a child is younger than its parent
                if p.ppid == parent && p.pid != parent && p.create >= pc && !v.contains(&(p.pid, p.create)) {
                    v.push((p.pid, p.create));
                }
            }
        }
        i += 1;
    }
    v
}

/// Rows built by one `rebuild`.
struct Out {
    rows: Vec<Row>,
    ids: Vec<Option<RowId>>,
    gone: Vec<Gone>,
}

/// Child rows of one expanded group from its held slots: processes that are still
/// running are live, actionable rows (wherever they belong now); the ones that
/// left are inert ghosts.
fn push_kids(out: &mut Out, slots: Vec<Slot<(u32, i64), usize>>, snap: &Snapshot, gi_of: &[usize], prev_names: &HashMap<(u32, i64), String>, names: &mut HashMap<(u32, i64), String>) {
    for slot in slots {
        match slot {
            Slot::Live(k) => {
                let p = &snap.procs[k];
                names.insert((p.pid, p.create), p.name.to_string());
                out.rows.push(Row::Child(gi_of[k], k));
                out.ids.push(Some((p.pid, p.create, false)));
            }
            Slot::Gone(k) => {
                let name = prev_names.get(&k).cloned().unwrap_or_else(|| "process".into());
                names.insert(k, name.clone());
                out.rows.push(Row::Gone(out.gone.len()));
                out.ids.push(None);
                out.gone.push(Gone { key: k, name, child: true, alive: false });
            }
        }
    }
}

#[derive(PartialEq)]
enum SortKey {
    F(f64),
    S(String),
}
impl Eq for SortKey {}
impl PartialOrd for SortKey {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for SortKey {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match (self, o) {
            (SortKey::F(a), SortKey::F(b)) => a.partial_cmp(b).unwrap_or(Equal),
            (SortKey::S(a), SortKey::S(b)) => a.cmp(b),
            (SortKey::F(_), SortKey::S(_)) => Less,
            (SortKey::S(_), SortKey::F(_)) => Greater,
        }
    }
}

fn win_dir() -> String {
    std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into()).to_lowercase() + "\\"
}

/// Is this one of our own images: the installed exe, a staging / parked / multi
/// copy, the Task Manager launcher, or the pre-rename `blink.exe` while an old
/// install still runs. Presentation only: it decides what counts as "Hayate"
/// when grouping, never what may be started or ended.
pub fn is_own_image(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    let stem = n.strip_suffix(".exe").unwrap_or(&n);
    for base in ["hayate", "blink"] {
        let Some(rest) = stem.strip_prefix(base) else { continue };
        if rest.is_empty() || matches!(rest, ".new" | ".old" | ".bad") {
            return true;
        }
        if let Some(r) = rest.strip_prefix('-') {
            if r == "multi" || r == "tm" || r.starts_with("tm.") {
                return true;
            }
            if !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// Groups processes the way Task Manager does: chains of same-named
/// processes collapse into their topmost ancestor; non-windowed children of
/// an app (except under Explorer) join that app.
///
/// One exception: a program Hayate started at sign-in is its own program, not
/// part of Hayate. Its helpers (a console host, a crash handler) stay with it,
/// so the Hayate row shows Hayate's own memory instead of the sum of everything
/// it launched.
pub fn build_groups(snap: &Snapshot) -> Vec<Group> {
    let procs = &snap.procs;
    let own: Vec<bool> = procs.iter().map(|p| is_own_image(&p.name)).collect();
    let by_pid: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.pid, i)).collect();
    let parent = |i: usize| -> Option<usize> {
        let p = &procs[i];
        let j = *by_pid.get(&p.ppid)?;
        (j != i && procs[j].create <= p.create && p.pid > 4).then_some(j)
    };
    // same-name root
    let mut root: Vec<usize> = (0..procs.len()).collect();
    let mut order: Vec<usize> = (0..procs.len()).collect();
    order.sort_by_key(|&i| procs[i].create);
    for &i in &order {
        if let Some(pi) = parent(i) {
            if procs[pi].name.eq_ignore_ascii_case(&procs[i].name) {
                root[i] = root[pi];
            }
        }
    }
    // app membership: windowed roots are apps
    let windowed_root: HashSet<usize> = (0..procs.len()).filter(|&i| procs[i].windowed).map(|i| root[i]).collect();
    // attach non-windowed child roots to an app parent (not explorer). What we
    // started ourselves stays out of our group unless it is another copy of us.
    for &i in &order {
        if root[i] != i || windowed_root.contains(&i) {
            continue;
        }
        if let Some(pi) = parent(i) {
            let pr = root[pi];
            // a program we started is not part of us; a child of one of our own
            // helpers (launcher, staging copy) still is
            let foreign_under_us = own[pr] && !own[i] && pi == pr;
            if windowed_root.contains(&pr) && !procs[pr].name.eq_ignore_ascii_case("explorer.exe") && !foreign_under_us {
                root[i] = pr;
            }
        }
    }
    // the helpers of a program we started belong with that program: walk up to
    // the nearest ancestor that is a direct child of one of our images
    for &i in &order {
        if root[i] != i || windowed_root.contains(&i) || own[i] {
            continue;
        }
        let mut a = parent(i);
        let mut guard = 0;
        while let Some(pi) = a {
            guard += 1;
            if guard > 64 {
                break;
            }
            if own[pi] {
                break;
            }
            if parent(pi).is_some_and(|g| own[g]) {
                if root[pi] == pi {
                    root[i] = pi;
                }
                break;
            }
            a = parent(pi);
        }
    }
    // resolve chains (a child root may itself have been re-rooted)
    for &i in &order {
        let mut r = root[i];
        let mut guard = 0;
        while root[r] != r && guard < 64 {
            r = root[r];
            guard += 1;
        }
        root[i] = r;
    }
    let wd = win_dir();
    let mut map: HashMap<usize, Group> = HashMap::new();
    for (i, p) in procs.iter().enumerate() {
        let r = root[i];
        let g = map.entry(r).or_insert_with(|| {
            let rp = &procs[r];
            let kind = if windowed_root.contains(&r) {
                Kind::App
            } else if rp.pid <= 4
                || rp.meta.path.as_deref().map(|p| p.to_lowercase().starts_with(&wd)).unwrap_or(rp.meta.path.is_none() && rp.session == 0)
            {
                Kind::Windows
            } else {
                Kind::Background
            };
            let mut name = match (&rp.meta.desc, kind) {
                (Some(d), _) if !d.is_empty() => d.to_string(),
                _ => rp.name.to_string(),
            };
            if rp.meta.wow64 {
                name.push_str(" (32 bit)");
            }
            Group { key: (rp.pid, rp.create), name, kind, members: vec![], cpu: 0.0, mem: 0, io: 0.0, gpu: 0.0, gpu_engine: None, gpu_top: 0.0, threads: 0, handles: 0 }
        });
        g.members.push(i);
        g.cpu += p.cpu;
        g.mem += p.mem;
        g.io += p.io_bps;
        g.gpu += p.gpu;
        if p.gpu > g.gpu_top {
            g.gpu_top = p.gpu;
            g.gpu_engine = p.gpu_engine.clone();
        }
        g.threads += p.threads;
        g.handles += p.handles;
    }
    map.into_values().collect()
}

pub fn fmt_mem(b: u64) -> String {
    let mb = b as f64 / 1048576.0;
    if mb >= 10240.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else if mb >= 100.0 {
        format!("{:.0} MB", mb)
    } else {
        format!("{:.1} MB", mb)
    }
}
fn fmt_io(b: f64) -> String {
    let m = b / 1e6;
    if m >= 0.05 {
        format!("{:.1} MB/s", m)
    } else {
        "0 MB/s".into()
    }
}

pub(super) fn name_color(s: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for b in s.bytes() {
        h = (h ^ b.to_ascii_lowercase() as u32).wrapping_mul(16777619);
    }
    const P: [u32; 8] = [C_NET, C_CPU, C_MEM, C_GPU, C_DSK, 0xe98a6f, 0xd884b4, 0x8fa3b8];
    P[(h % 8) as usize]
}

pub fn paint(app: &mut App, g: &mut Gfx, body: Rect) {
    let snap = app.snap.clone();
    if app.procs.dirty {
        let q = app.search.clone();
        let ix = Interact {
            now: order::now_ms(),
            // the pointer on the open Name card still counts: the order must not move under it
            inside: app.mx >= 0.0 && (app.procs.list.contains(app.mx, app.my) || app.hov.card_contains(app.mx, app.my)),
            active: app.down.is_some() || app.menu_open,
        };
        app.procs.rebuild(&snap, &q, ix);
    }
    let pv = &mut app.procs;
    let icon_px = (16.0 * g.dpi / 96.0).round() as u32;

    // keyboard movement, relative to the selected row's identity in the current rows
    let cur = pv.row_ids.iter().position(|r| r.is_some() && *r == pv.sel);
    if pv.pending_move != 0 {
        let d = pv.pending_move;
        pv.pending_move = 0;
        let mut i = cur.map(|c| c as i64).unwrap_or(-1) + d as i64;
        // an empty list (a search that matches nothing) has no last row: clamp(0, -1) panics
        i = i.clamp(0, (pv.rows.len() as i64 - 1).max(0));
        // skip headers and exited slots in the direction of travel
        let n = pv.rows.len() as i64;
        let step = if d < 0 { -1 } else { 1 };
        let skip = |mut i: i64, step: i64| {
            while (0..n).contains(&i) && pv.row_ids[i as usize].is_none() {
                i += step;
            }
            i
        };
        let fwd = skip(i, step);
        i = if (0..n).contains(&fwd) { fwd } else { skip(i, -step) };
        if (0..n).contains(&i) {
            pv.sel = pv.row_ids[i as usize];
            let y = i as f32 * ROW_H;
            if y < pv.scroll {
                pv.scroll = y;
            } else if y + ROW_H > pv.scroll + pv.view_h {
                pv.scroll = y + ROW_H - pv.view_h;
            }
        }
    }

    let area = body;
    let head = Rect::new(area.x, area.y, area.w - 10.0, HEAD_H);
    let list = Rect::new(area.x, area.y + HEAD_H, area.w - 10.0, area.h - HEAD_H);
    pv.view_h = list.h;
    pv.list = list;
    pv.clamp();
    // "Show in Processes": bring the row into the middle of the list once, whatever the order did
    if let Some(id) = pv.reveal.take() {
        if let Some(i) = pv.row_ids.iter().position(|r| *r == Some(id)) {
            pv.sel = Some(id);
            let mid = (pv.view_h.max(ROW_H) - ROW_H) / 2.0;
            pv.scroll = (i as f32 * ROW_H - mid).max(0.0);
            pv.clamp();
        }
    }
    let on = pv.on_cols();
    let total_w: f32 = on.iter().map(|&i| pv.cols[i].w).sum();
    pv.hs = pv.hs.clamp(0.0, (total_w - head.w + 16.0).max(0.0));
    let x0 = area.x + 8.0 - pv.hs;
    pv.cols_x.clear();
    {
        let mut x = x0;
        for &ci in &on {
            pv.cols_x.push((x, x + pv.cols[ci].w, ci));
            x += pv.cols[ci].w;
        }
    }

    // totals for header
    let s = &snap;
    let mem_pct = if s.mem.total > 0 { (s.mem.total - s.mem.avail) as f32 / s.mem.total as f32 * 100.0 } else { 0.0 };
    let disk_pct = s.disks.iter().map(|d| d.sub.split(" · ").nth(1).and_then(|b| b.trim_end_matches("% busy").parse::<f32>().ok()).unwrap_or(0.0)).fold(0.0, f32::max);
    let gpu_pct = s.gpus.first().map(|g| g.value as f32).unwrap_or(0.0);

    // saved rules, only when the Status column shows them (one copy per paint, no lock held)
    let rview = if on.iter().any(|&ci| pv.cols[ci].col == Col::Status) { crate::features::get().map(|f| f.rules.view()) } else { None };

    // rows
    let row_hits = app.hits.len();
    g.clip(list);
    let first = (pv.scroll / ROW_H).floor() as usize;
    let last = ((pv.scroll + list.h) / ROW_H).ceil() as usize + 1;
    for ri in first..last.min(pv.rows.len()) {
        let ry = list.y + ri as f32 * ROW_H - pv.scroll;
        let rr = Rect::new(list.x, ry, list.w, ROW_H);
        let row = pv.rows[ri];
        if let Row::Header(kind, n) = row {
            let t = match kind {
                Kind::App => "Apps",
                Kind::Background => "Background processes",
                Kind::Windows => "Windows processes",
            };
            g.text(&format!("{t} ({n})"), Rect::new(list.x + 10.0, ry + 3.0, 400.0, ROW_H), f(13.0).b(), TX);
            continue;
        }
        if let Row::Gone(gi) = row {
            // exited while the order is held: slot kept, dim, no hit target, no actions
            let gn = &pv.gone[gi];
            if pv.sel == Some((gn.key.0, gn.key.1, !gn.child)) {
                g.fill(rr, ACC_BG);
            }
            let mut x = x0;
            for &ci in &on {
                let c = &pv.cols[ci];
                let cr = Rect::new(x, ry, c.w, ROW_H);
                x += c.w;
                if cr.r() < list.x || cr.x > list.r() {
                    continue;
                }
                let pad = cr.inset(8.0, 0.0);
                match c.col {
                    Col::Name => {
                        let tx = pad.x + if gn.child { 35.0 } else { 16.0 } + 19.0;
                        g.round(Rect::new(tx - 19.0, ry + 5.0, 12.0, 12.0), 3.0, TRACK);
                        g.text(&gn.name, Rect::new(tx, ry, (pad.r() - tx).max(0.0), ROW_H), f(12.0), TX3);
                    }
                    Col::Status => g.text(if gn.alive { "Moved" } else { "Exited" }, pad, f(11.5), TX4),
                    _ => {}
                }
            }
            g.fill(Rect::new(list.x, rr.b() - 1.0, list.w, 1.0), SEP);
            continue;
        }
        let key = pv.row_ids[ri];
        let selected = key.is_some() && key == pv.sel;
        let hovered = rr.contains(app.mx, app.my);
        if selected {
            g.fill(rr, ACC_BG);
        } else if hovered {
            g.fill_a(rr, 0xffffff, 0.035);
        }
        app.hits.push((rr, Hit::Row(ri)));
        let rm = rview.as_ref().map(|v| super::rulepeek::row_mark(v, &snap, &pv.groups, row)).unwrap_or(GroupMark::Plain);
        let badge_hov = app.hover == Some(Hit::RuleBadge(ri));
        let mut x = x0;
        for &ci in &on {
            let c = &pv.cols[ci];
            let cr = Rect::new(x, ry, c.w, ROW_H);
            x += c.w;
            if cr.r() < list.x || cr.x > list.r() {
                continue;
            }
            cell(app_hits(&mut app.hits), g, &mut app.icons, icon_px, c, cr, row, &pv.groups, &snap, &pv.expanded, ri, mem_pct, (rm, badge_hov));
        }
        g.fill(Rect::new(list.x, rr.b() - 1.0, list.w, 1.0), SEP);
    }
    g.unclip();
    // Drawing clips do not clip hit regions (including badges and partial rows).
    let mut i = row_hits;
    while i < app.hits.len() {
        if let Some(r) = super::cpu::visible_rect(app.hits[i].0, list) {
            app.hits[i].0 = r;
            i += 1;
        } else { app.hits.remove(i); }
    }

    // header (drawn after rows so it sits on top)
    g.fill(Rect::new(area.x, area.y, area.w, HEAD_H), BG);
    g.fill(Rect::new(area.x, area.y + HEAD_H - 1.0, area.w, 1.0), LN);
    g.clip(head);
    let mut x = x0;
    for &ci in &on {
        let c = &pv.cols[ci];
        let hr = Rect::new(x, head.y, c.w, HEAD_H);
        x += c.w;
        let sorted = pv.sort == c.col;
        let tot = match c.col {
            Col::Cpu => Some(format!("{:.0}%", s.cpu)),
            Col::Mem => Some(format!("{mem_pct:.0}%")),
            Col::Disk => Some(format!("{disk_pct:.0}%")),
            Col::Gpu => Some(format!("{gpu_pct:.0}%")),
            _ => None,
        };
        let pad = hr.inset(8.0, 0.0);
        let fnt = if c.right { f(11.0).r() } else { f(11.0) };
        if let Some(t) = tot {
            g.text(&t, Rect::new(pad.x, hr.y + 4.0, pad.w, 18.0), if c.right { f(14.0).m().r() } else { f(14.0).m() }, TX);
        }
        let label = if sorted { format!("{} {}", c.label, if pv.desc { "▾" } else { "▴" }) } else { c.label.to_string() };
        g.text(&label, Rect::new(pad.x, hr.y + 21.0, pad.w, 16.0), fnt, if sorted { TX } else { DIM });
        app.hits.push((hr, Hit::Header(ci)));
        g.fill(Rect::new(hr.r() - 1.0, hr.y + 8.0, 1.0, HEAD_H - 16.0), LN);
        app.hits.push((Rect::new(hr.r() - 4.0, hr.y, 8.0, HEAD_H), Hit::Resize(ci)));
    }
    g.unclip();

    // scrollbar
    if pv.content_h > list.h {
        let track = Rect::new(area.r() - 8.0, list.y + 2.0, 6.0, list.h - 4.0);
        pv.track = track;
        let th = (list.h / pv.content_h * track.h).max(24.0);
        let ty = track.y + pv.scroll / (pv.content_h - list.h) * (track.h - th);
        let hov = app.hover == Some(Hit::Scrollbar) || app.down == Some(Hit::Scrollbar);
        g.round(Rect::new(track.x, ty, track.w, th), 3.0, if hov { 0x3a414d } else { LN });
        app.hits.push((track, Hit::Scrollbar));
    }

    // hover tip of the rules badge under the pointer (built only for that one row)
    if let (Some(Hit::RuleBadge(ri)), true) = (app.hover, app.down.is_none()) {
        let at = app.hits.iter().find(|(_, h)| *h == Hit::RuleBadge(ri)).map(|(r, _)| *r);
        if let Some(rect) = at.filter(|r| r.contains(app.mx, app.my)) {
            if let Some(paras) = super::rulepeek::badge_tip(app, ri) {
                app.set_tip(rect, paras);
            }
        }
    }
}

/// The process's own icon (16 DIP, centred where the 12 DIP placeholder
/// square sits), or that square while the icon loads or when there is none.
fn proc_icon(g: &mut Gfx, icons: &mut super::icons::Icons, size: u32, p: &Proc, x: f32, r: Rect) {
    if let Some(path) = p.meta.path.as_deref() {
        if let Some((key, px)) = icons.get(path, p.pid, size) {
            g.image(&key, px.w, px.h, &px.data, Rect::new(x - 2.0, r.y + 3.0, 16.0, 16.0));
            return;
        }
    }
    g.round(Rect::new(x, r.y + 5.0, 12.0, 12.0), 3.0, name_color(&p.name));
}

// borrow helper: lets `cell` push hit regions while `app.procs` is borrowed
fn app_hits(h: &mut Vec<(Rect, Hit)>) -> &mut Vec<(Rect, Hit)> {
    h
}

#[allow(clippy::too_many_arguments)]
fn cell(
    hits: &mut Vec<(Rect, Hit)>,
    g: &mut Gfx,
    icons: &mut super::icons::Icons,
    icon_px: u32,
    c: &ColState,
    r: Rect,
    row: Row,
    groups: &[Group],
    snap: &Snapshot,
    expanded: &HashSet<(u32, i64)>,
    ri: usize,
    _mem_pct: f32,
    rule: (GroupMark, bool),
) {
    let pad = r.inset(8.0, 0.0);
    let (grp, proc_i) = match row {
        Row::Group(gi) => (Some(&groups[gi]), groups[gi].members[0]),
        Row::Child(_, pi) => (None, pi),
        Row::Header(..) | Row::Gone(_) => return,
    };
    let p = &snap.procs[proc_i];
    let single = grp.map(|g| g.members.len() == 1).unwrap_or(true);
    match c.col {
        Col::Name => {
            let wl = row_workload(grp, snap);
            let mut x = pad.x;
            if let Some(gr) = grp {
                if gr.members.len() > 1 {
                    let open = expanded.contains(&gr.key);
                    let cr = Rect::new(x - 4.0, r.y, 18.0, r.h);
                    g.text(if open { "\u{E70D}" } else { "\u{E76C}" }, cr, f(8.5).icon().c(), DIM);
                    hits.push((cr, Hit::Chevron(ri)));
                }
                x += 16.0;
                proc_icon(g, icons, icon_px, p, x, r);
                x += 19.0;
                let base = wl.as_ref().map(|w| w.label.clone()).unwrap_or_else(|| gr.name.clone());
                let label = if gr.members.len() > 1 { format!("{base} ({})", gr.members.len()) } else { base };
                name_text(g, &label, wl.as_deref(), Rect::new(x, r.y, pad.r() - x, r.h), f(12.0), TX);
            } else {
                x += 35.0;
                proc_icon(g, icons, icon_px, p, x, r);
                x += 19.0;
                let label = p.meta.workload.as_ref().map(|w| w.label.clone()).unwrap_or_else(|| p.name.to_string());
                name_text(g, &label, p.meta.workload.as_deref(), Rect::new(x, r.y, pad.r() - x, r.h), f(12.0), TX2);
            }
        }
        // Rules badge (exited / moved rows draw their own words in `paint`; the badge sits inside this cell only)
        Col::Status => super::rulepeek::badge(hits, g, r, ri, rule.0, rule.1),
        col => {
            if let Some(v) = heat_of(col, grp, p) {
                let a = (v.clamp(0.0, 1.0)) * 0.38 + 0.04;
                g.fill_a(Rect::new(r.x, r.y, r.w - 1.0, r.h - 1.0), 0x7c5cff, a);
            }
            if let Some(t) = cell_text(col, grp, p, single) {
                g.text(&t.s, pad, t.font, t.color);
            }
        }
    }
}

/// What a text cell shows and how: the one place the string and font of a cell are decided, so
/// the full-text tooltip of a clipped cell is exactly what the cell draws.
pub struct CellText {
    pub s: String,
    pub font: Font,
    pub color: u32,
}

/// Cell text for every column except Name (own drawing + card) and Status (rules badge + its tip).
pub fn cell_text(col: Col, grp: Option<&Group>, p: &Proc, single: bool) -> Option<CellText> {
    let dim = |s: &str| CellText { s: s.to_string(), font: f(11.5), color: DIM };
    let num = |s: String| CellText { s, font: f(11.5).m().r(), color: TX };
    Some(match col {
        Col::Name | Col::Status => return None,
        Col::Type => dim(if grp.map(|g| g.kind == Kind::App).unwrap_or(false) { "App" } else { "Process" }),
        Col::Publisher => dim(p.meta.publisher.as_deref().unwrap_or("")),
        Col::Pid => {
            if !single {
                return None;
            }
            CellText { s: p.pid.to_string(), font: f(11.5).m().r(), color: DIM }
        }
        Col::ProcName => dim(&p.name),
        Col::CmdLine => {
            let s = if single || grp.is_none() { p.meta.cmd.as_deref() } else { p.meta.path.as_deref() };
            CellText { s: s.unwrap_or("").to_string(), font: f(11.0).m(), color: DIM }
        }
        Col::Path => CellText { s: p.meta.path.as_deref().unwrap_or("").to_string(), font: f(11.0).m(), color: DIM },
        Col::Cpu => num(format!("{:.1}%", grp.map(|g| g.cpu).unwrap_or(p.cpu))),
        Col::Mem => num(fmt_mem(grp.map(|g| g.mem).unwrap_or(p.mem))),
        Col::Disk => num(fmt_io(grp.map(|g| g.io).unwrap_or(p.io_bps))),
        Col::Gpu => num(format!("{:.1}%", grp.map(|g| g.gpu).unwrap_or(p.gpu).min(100.0))),
        Col::GpuEngine => dim(if let Some(gr) = grp { gr.gpu_engine.as_deref() } else { p.gpu_engine.as_deref() }.unwrap_or("")),
        Col::Threads => num(grp.map(|g| g.threads).unwrap_or(p.threads).to_string()),
        Col::Handles => num(grp.map(|g| g.handles).unwrap_or(p.handles).to_string()),
    })
}

/// 0..1 heat of a numeric cell (None: not a heat column).
fn heat_of(col: Col, grp: Option<&Group>, p: &Proc) -> Option<f32> {
    Some(match col {
        Col::Cpu => grp.map(|g| g.cpu).unwrap_or(p.cpu) / 8.0,
        Col::Mem => grp.map(|g| g.mem).unwrap_or(p.mem) as f32 / 3e9,
        Col::Disk => (grp.map(|g| g.io).unwrap_or(p.io_bps) / 5e6) as f32,
        Col::Gpu => grp.map(|g| g.gpu).unwrap_or(p.gpu).min(100.0) / 60.0,
        _ => return None,
    })
}

/// The workload a group row stands for: its members' when every member has the same one.
/// A group of different workloads (or some unknown) has none: it keeps its plain name and
/// the card says it is mixed. A single-member group is that member's.
pub fn row_workload(grp: Option<&Group>, snap: &Snapshot) -> Option<Arc<crate::sample::workload::Workload>> {
    let g = grp?;
    let mut it = g.members.iter().map(|&i| snap.procs.get(i).and_then(|p| p.meta.workload.clone()));
    let first = it.next()??;
    it.all(|w| w.as_deref() == Some(&*first)).then_some(first)
}

/// Name text, with the inferred workload's runtime as a small tag after it (dropped when the
/// column is too narrow for it). The tag is what marks the label as inferred, not an app's own name.
fn name_text(g: &mut Gfx, label: &str, wl: Option<&crate::sample::workload::Workload>, r: Rect, font: Font, color: u32) {
    let Some(w) = wl else {
        g.text(label, r, font, color);
        return;
    };
    let chip_w = g.measure(w.runtime, f(10.0)) + 12.0;
    if r.w < chip_w + 48.0 {
        g.text(label, r, font, color);
        return;
    }
    let tw = g.measure(label, font).min(r.w - chip_w - 6.0);
    g.text(label, Rect::new(r.x, r.y, tw, r.h), font, color);
    let cr = Rect::new(r.x + tw + 6.0, r.y + 4.0, chip_w, 14.0);
    g.round_stroke(cr, 7.0, LN, 1.0);
    g.text(w.runtime, cr, f(10.0).c(), C_NET);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_in_does_not_enable_actions_before_target_is_revealed() {
        let mut pv = ProcView::new();
        pv.show((100, 1, false), Some((100, 1)));
        assert_eq!(pv.sel, None);
        assert_eq!(pv.reveal, Some((100, 1, false)));
        let s = snap(vec![proc(100, 1, "target.exe", 1.0)]);
        assert!(pv.selected_pids(&s).is_empty());
    }

    fn proc(pid: u32, create: i64, name: &str, cpu: f32) -> Proc {
        Proc { pid, ppid: 0, name: name.into(), create, cpu, mem: 1 << 20, io_bps: 0.0, threads: 1, handles: 1, session: 1, windowed: true, meta: Arc::new(Default::default()), gpu: 0.0, gpu_engine: None }
    }
    fn child(pid: u32, create: i64, ppid: u32, name: &str, cpu: f32) -> Proc {
        Proc { ppid, windowed: false, ..proc(pid, create, name, cpu) }
    }
    fn snap(procs: Vec<Proc>) -> Arc<Snapshot> {
        Arc::new(Snapshot { procs, ..Default::default() })
    }
    fn at(now: u64, inside: bool) -> Interact {
        Interact { now, inside, active: false }
    }
    /// identities of the real rows, in order
    fn ids(pv: &ProcView) -> Vec<(u32, i64)> {
        pv.row_ids.iter().flatten().map(|r| (r.0, r.1)).collect()
    }
    fn col(pv: &ProcView, c: Col) -> usize {
        pv.cols.iter().position(|x| x.col == c).unwrap()
    }

    #[test]
    fn order_updates_every_five_seconds_values_stay_live() {
        let mut pv = ProcView::new();
        pv.rebuild(&snap(vec![proc(1, 10, "a.exe", 50.0), proc(2, 20, "b.exe", 10.0)]), "", at(0, false));
        assert_eq!(ids(&pv), vec![(1, 10), (2, 20)]);
        // b overtakes a: shown order holds for 5 s, but the numbers are the new ones
        let s = snap(vec![proc(1, 10, "a.exe", 5.0), proc(2, 20, "b.exe", 90.0)]);
        pv.rebuild(&s, "", at(1000, false));
        assert_eq!(ids(&pv), vec![(1, 10), (2, 20)]);
        let Row::Group(gi) = pv.rows[2] else { panic!() };
        assert_eq!(pv.groups[gi].cpu, 90.0, "values are live while order is held");
        pv.rebuild(&s, "", at(5000, false));
        assert_eq!(ids(&pv), vec![(2, 20), (1, 10)]);
    }

    #[test]
    fn pointer_in_list_freezes_and_thaws_two_seconds_after_leaving() {
        let mut pv = ProcView::new();
        pv.rebuild(&snap(vec![proc(1, 10, "a.exe", 50.0), proc(2, 20, "b.exe", 10.0)]), "", at(0, true));
        let flipped = snap(vec![proc(1, 10, "a.exe", 5.0), proc(2, 20, "b.exe", 90.0)]);
        for t in (1000..=20_000).step_by(1000) {
            pv.rebuild(&flipped, "", at(t, true));
            assert_eq!(ids(&pv), vec![(1, 10), (2, 20)], "t={t}");
        }
        pv.rebuild(&flipped, "", at(21_000, false));
        pv.rebuild(&flipped, "", at(22_000, false));
        assert_eq!(ids(&pv), vec![(1, 10), (2, 20)], "grace not over");
        pv.rebuild(&flipped, "", at(23_000, false));
        assert_eq!(ids(&pv), vec![(2, 20), (1, 10)]);
    }

    #[test]
    fn menu_or_drag_freezes_like_the_pointer() {
        let mut pv = ProcView::new();
        pv.rebuild(&snap(vec![proc(1, 10, "a.exe", 50.0), proc(2, 20, "b.exe", 10.0)]), "", at(0, false));
        let flipped = snap(vec![proc(1, 10, "a.exe", 5.0), proc(2, 20, "b.exe", 90.0)]);
        for t in (1000..=30_000).step_by(1000) {
            pv.rebuild(&flipped, "", Interact { now: t, inside: false, active: true });
            assert_eq!(ids(&pv), vec![(1, 10), (2, 20)], "t={t}");
        }
        pv.rebuild(&flipped, "", at(31_000, false));
        pv.rebuild(&flipped, "", at(32_000, false));
        assert_eq!(ids(&pv), vec![(1, 10), (2, 20)]);
        pv.rebuild(&flipped, "", at(33_000, false));
        assert_eq!(ids(&pv), vec![(2, 20), (1, 10)]);
    }

    #[test]
    fn explicit_sort_applies_at_once_even_under_the_pointer() {
        let mut pv = ProcView::new();
        let s = snap(vec![proc(1, 10, "zeta.exe", 50.0), proc(2, 20, "alpha.exe", 10.0)]);
        pv.rebuild(&s, "", at(0, true));
        assert_eq!(ids(&pv), vec![(1, 10), (2, 20)]);
        let name = col(&pv, Col::Name);
        pv.sort_by(name);
        pv.rebuild(&s, "", at(500, true));
        assert_eq!(ids(&pv), vec![(2, 20), (1, 10)], "name ascending, immediately");
        // and that is the new stable order
        let flipped = snap(vec![proc(1, 10, "zeta.exe", 5.0), proc(2, 20, "alpha.exe", 95.0)]);
        pv.rebuild(&flipped, "", at(1500, true));
        assert_eq!(ids(&pv), vec![(2, 20), (1, 10)]);
    }

    #[test]
    fn filter_change_applies_at_once() {
        let mut pv = ProcView::new();
        let s = snap(vec![proc(1, 10, "a.exe", 50.0), proc(2, 20, "b.exe", 10.0)]);
        pv.rebuild(&s, "", at(0, true));
        pv.rebuild(&s, "b.exe", at(100, true));
        assert_eq!(ids(&pv), vec![(2, 20)]);
    }

    /// A hidden window's tick publishes `procs: []` but keeps the last `nprocs`; showing the window
    /// paints that snapshot once, a moment before the first real one (header already says 699).
    #[test]
    fn list_painted_empty_once_fills_with_the_first_real_snapshot() {
        for (mode, inside, active) in [("quiet", false, false), ("pointer", true, false), ("menu", false, true)] {
            let mut pv = ProcView::new();
            let ix = |now| Interact { now, inside, active };
            pv.rebuild(&snap(vec![]), "", ix(0));
            assert!(pv.rows.is_empty(), "{mode}: nothing to show yet");
            let live = snap(vec![proc(1, 10, "a.exe", 50.0), proc(2, 20, "b.exe", 10.0)]);
            pv.rebuild(&live, "", ix(1000));
            assert_eq!(ids(&pv), vec![(1, 10), (2, 20)], "{mode}: first real snapshot must populate at once");
            // and the order it adopted is a normal held one
            let flipped = snap(vec![proc(1, 10, "a.exe", 5.0), proc(2, 20, "b.exe", 90.0)]);
            pv.rebuild(&flipped, "", ix(2000));
            assert_eq!(ids(&pv), vec![(1, 10), (2, 20)], "{mode}: held after the first fill");
        }
    }

    #[test]
    fn large_cold_start_under_the_pointer_shows_every_process() {
        let mut pv = ProcView::new();
        pv.rebuild(&snap(vec![]), "", at(0, true));
        let many = snap((1..=699).map(|i| proc(i, i as i64, &format!("p{i:03}.exe"), i as f32 % 7.0)).collect());
        pv.rebuild(&many, "", at(300, true));
        assert_eq!(ids(&pv).len(), 699);
        assert_eq!(pv.content_h, 700.0 * ROW_H, "one header + 699 rows");
    }

    #[test]
    fn search_that_matched_nothing_fills_when_rows_arrive_under_the_pointer() {
        // async metadata (publisher / description) can make a search match late
        let mut pv = ProcView::new();
        let s = snap(vec![proc(1, 10, "a.exe", 5.0)]);
        pv.rebuild(&s, "pub", at(0, true));
        assert!(pv.rows.is_empty());
        let mut p = proc(1, 10, "a.exe", 5.0);
        p.meta = Arc::new(crate::sample::meta::Meta { publisher: Some("Pub Corp".into()), ..Default::default() });
        pv.rebuild(&snap(vec![p]), "pub", at(1000, true));
        assert_eq!(ids(&pv), vec![(1, 10)]);
    }

    /// The narrowing that keeps the pointer rule: a section that is empty while OTHER rows are
    /// on screen is not a blank list; a new section (header + rows) must not push rows down.
    #[test]
    fn new_section_still_waits_while_other_rows_are_under_the_pointer() {
        let mut pv = ProcView::new();
        let bg = |pid: u32, create: i64, name: &str| Proc { windowed: false, session: 1, ..proc(pid, create, name, 1.0) };
        pv.rebuild(&snap(vec![bg(1, 10, "svc.exe")]), "", at(0, true));
        let before = ids(&pv);
        assert_eq!(before, vec![(1, 10)]);
        // an app window opens: its section would sit ABOVE the Background rows
        pv.rebuild(&snap(vec![bg(1, 10, "svc.exe"), proc(2, 20, "new-app.exe", 50.0)]), "", at(1000, true));
        assert_eq!(ids(&pv), before, "new section waits for the pointer to leave");
        pv.rebuild(&snap(vec![bg(1, 10, "svc.exe"), proc(2, 20, "new-app.exe", 50.0)]), "", at(30_000, false));
        pv.rebuild(&snap(vec![bg(1, 10, "svc.exe"), proc(2, 20, "new-app.exe", 50.0)]), "", at(32_000, false));
        assert_eq!(ids(&pv), vec![(2, 20), (1, 10)]);
    }

    #[test]
    fn pid_reuse_keeps_a_dead_slot_and_the_new_process_waits() {
        let mut pv = ProcView::new();
        pv.rebuild(&snap(vec![proc(100, 1, "old.exe", 30.0), proc(200, 2, "keep.exe", 10.0)]), "", at(0, true));
        // 100 exits and the OS hands pid 100 to a new process, busier than anything
        let s = snap(vec![proc(100, 99, "new.exe", 80.0), proc(200, 2, "keep.exe", 10.0)]);
        pv.rebuild(&s, "", at(1000, true));
        assert_eq!(pv.rows.len(), 3);
        assert_eq!(pv.rows[1], Row::Gone(0), "same slot, marked gone");
        assert_eq!(pv.gone[0].name, "old.exe");
        assert_eq!(pv.id_at(1), None, "a dead slot has no identity to act on");
        assert_eq!(ids(&pv), vec![(200, 2)], "the new pid-100 process is not inserted under the pointer");
        // pointer leaves, 2 s grace, then the order may update
        pv.rebuild(&s, "", at(2000, false));
        pv.rebuild(&s, "", at(3000, false));
        pv.rebuild(&s, "", at(4000, false));
        assert_eq!(pv.gone.len(), 1, "still held");
        pv.rebuild(&s, "", at(5000, false));
        assert!(pv.gone.is_empty());
        assert_eq!(ids(&pv), vec![(100, 99), (200, 2)]);
    }

    #[test]
    fn selection_of_a_replaced_pid_does_not_act_on_the_new_process() {
        let mut pv = ProcView::new();
        pv.rebuild(&snap(vec![proc(100, 1, "old.exe", 30.0), proc(200, 2, "keep.exe", 10.0)]), "", at(0, true));
        pv.select_row(1);
        assert_eq!(pv.sel, Some((100, 1, true)));
        let latest = snap(vec![proc(100, 99, "new.exe", 80.0), proc(200, 2, "keep.exe", 10.0)]);
        assert!(pv.selected_pids(&latest).is_empty(), "same pid, other create time: no target");
        let same = snap(vec![proc(100, 1, "old.exe", 1.0)]);
        assert_eq!(pv.selected_pids(&same).len(), 1);
    }

    #[test]
    fn selected_group_is_revalidated_member_by_member() {
        let mut pv = ProcView::new();
        let built = snap(vec![proc(10, 1, "app.exe", 5.0), child(11, 2, 10, "app.exe", 0.0), child(12, 3, 10, "helper.exe", 0.0)]);
        pv.rebuild(&built, "", at(0, false));
        pv.select_row(1);
        assert_eq!(pv.sel, Some((10, 1, true)));
        assert_eq!(pv.selected_pids(&built).len(), 3);
        // 12 exited, 11's pid was reused
        let latest = snap(vec![proc(10, 1, "app.exe", 5.0), proc(11, 77, "other.exe", 0.0)]);
        let got: Vec<_> = pv.selected_pids(&latest).into_iter().map(|t| (t.0, t.1)).collect();
        assert_eq!(got, vec![(10, 1)]);
    }

    #[test]
    fn release_only_counts_on_the_row_that_was_pressed() {
        let mut pv = ProcView::new();
        let s = snap(vec![proc(1, 10, "zeta.exe", 50.0), proc(2, 20, "alpha.exe", 10.0)]);
        pv.rebuild(&s, "", at(0, true));
        pv.press_row(1);
        assert!(pv.same_press(1));
        assert!(!pv.same_press(2));
        // a sort click reshuffles between press and release: row 1 is someone else now
        let name = col(&pv, Col::Name);
        pv.sort_by(name);
        pv.rebuild(&s, "", at(100, true));
        assert!(!pv.same_press(1), "index 1 is a different process now");
        assert!(pv.same_press(2), "the pressed process moved with its identity");
        assert!(!pv.same_press(0), "a header is no target");
    }

    #[test]
    fn dead_slot_cannot_be_selected_or_toggled() {
        let mut pv = ProcView::new();
        pv.rebuild(&snap(vec![proc(1, 10, "a.exe", 50.0), proc(2, 20, "b.exe", 10.0)]), "", at(0, true));
        pv.rebuild(&snap(vec![proc(2, 20, "b.exe", 10.0)]), "", at(1000, true));
        assert_eq!(pv.rows[1], Row::Gone(0));
        pv.select_row(1);
        assert_eq!(pv.sel, None);
        pv.toggle_row(1);
        assert!(pv.expanded.is_empty());
    }

    #[test]
    fn expanded_children_hold_their_order_too() {
        let mut pv = ProcView::new();
        let procs = |c11: f32, c12: f32| snap(vec![proc(10, 1, "app.exe", 0.0), child(11, 2, 10, "app.exe", c11), child(12, 3, 10, "helper.exe", c12)]);
        pv.expanded.insert((10, 1));
        pv.rebuild(&procs(9.0, 1.0), "", at(0, true));
        // group row, then its processes (the root is one of them)
        assert_eq!(ids(&pv), vec![(10, 1), (11, 2), (12, 3), (10, 1)]);
        pv.rebuild(&procs(1.0, 9.0), "", at(1000, true));
        assert_eq!(ids(&pv), vec![(10, 1), (11, 2), (12, 3), (10, 1)], "children keep their slots under the pointer");
        for t in 2..=6 {
            pv.rebuild(&procs(1.0, 9.0), "", at(t * 1000, false));
        }
        assert_eq!(ids(&pv), vec![(10, 1), (12, 3), (11, 2), (10, 1)]);
    }

    #[test]
    fn scroll_position_follows_the_process_when_the_order_updates() {
        let mut pv = ProcView::new();
        let many = |first_cpu: f32| {
            let mut v: Vec<Proc> = (1..=40).map(|i| proc(i, i as i64, &format!("p{i:02}.exe"), 40.0 - i as f32)).collect();
            v[0].cpu = first_cpu;
            snap(v)
        };
        pv.rebuild(&many(100.0), "", at(0, false));
        pv.content_h = pv.rows.len() as f32 * ROW_H;
        pv.view_h = 10.0 * ROW_H;
        pv.scroll = 5.0 * ROW_H;
        let at_top = pv.row_ids[5].unwrap();
        // process 1 drops to the bottom after the timer: rows above the view move, the viewed process does not
        for t in 1..=5 {
            pv.rebuild(&many(0.0), "", at(t * 1000, false));
        }
        let i = pv.row_ids.iter().position(|r| *r == Some(at_top)).unwrap();
        assert_eq!(pv.scroll, i as f32 * ROW_H - 0.0, "same process stays at the same place on screen");
    }

    // ---- frozen tree: structure survives exits and section changes ----------------------------

    /// compact picture of the rows: `H:<kind>:<n>`, `G<pid>` group, `C<pid>` live child,
    /// `x<pid>` exited root, `xc<pid>` exited child, `m<pid>` still running, no longer a group
    fn shape(pv: &ProcView) -> Vec<String> {
        let built = pv.built.as_ref().unwrap();
        pv.rows
            .iter()
            .map(|r| match *r {
                Row::Header(k, n) => format!("H:{k:?}:{n}"),
                Row::Group(gi) => format!("G{}", pv.groups[gi].key.0),
                Row::Child(_, k) => format!("C{}", built.procs[k].pid),
                Row::Gone(i) => {
                    let g = &pv.gone[i];
                    format!("{}{}{}", if g.alive { "m" } else { "x" }, if g.child { "c" } else { "" }, g.key.0)
                }
            })
            .collect()
    }
    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }
    /// ticks once a second with the pointer outside until `to`
    fn thaw(pv: &mut ProcView, s: &Arc<Snapshot>, from: u64, to: u64) {
        let mut t = from;
        while t <= to {
            pv.rebuild(s, "", at(t, false));
            t += 1000;
        }
    }
    fn app_with_two_children(c11: f32, c12: f32) -> Arc<Snapshot> {
        snap(vec![proc(10, 1, "app.exe", 0.0), child(11, 2, 10, "app.exe", c11), child(12, 3, 10, "helper.exe", c12)])
    }
    fn held(mode: u8, t: u64) -> Interact {
        if mode == 0 { at(t, true) } else { Interact { now: t, inside: false, active: true } }
    }

    #[test]
    fn expanded_group_keeps_its_block_when_children_exit_while_held() {
        for mode in 0..2 {
            let mut pv = ProcView::new();
            pv.expanded.insert((10, 1));
            pv.rebuild(&app_with_two_children(9.0, 1.0), "", held(mode, 0));
            assert_eq!(shape(&pv), strs(&["H:App:1", "G10", "C11", "C12", "C10"]), "block shown");
            // both children exit: the group is one process now, the user's block stays
            let one = snap(vec![proc(10, 1, "app.exe", 0.0)]);
            for t in 1..=12 {
                pv.rebuild(&one, "", held(mode, t * 1000));
                assert_eq!(shape(&pv), strs(&["H:App:1", "G10", "xc11", "xc12", "C10"]), "mode {mode} t={t}: ghosts hold the slots");
            }
            // only the group and the live process are targets; ghosts are inert
            assert_eq!(pv.row_ids.iter().flatten().count(), 2);
            for i in 0..pv.rows.len() {
                if matches!(pv.rows[i], Row::Gone(_)) {
                    assert_eq!(pv.id_at(i), None);
                }
            }
            // after the thaw the block collapses
            thaw(&mut pv, &one, 13_000, 22_000);
            assert_eq!(shape(&pv), strs(&["H:App:1", "G10"]));
            assert!(pv.gone.is_empty());
        }
    }

    #[test]
    fn root_exit_keeps_the_ghost_and_its_surviving_processes_until_thaw() {
        let mut pv = ProcView::new();
        pv.expanded.insert((10, 1));
        pv.rebuild(&app_with_two_children(5.0, 1.0), "", at(0, true));
        // the root exits; 11 and 12 live on (parentless now). The pointer is still in the list.
        let orphans = snap(vec![child(11, 2, 10, "app.exe", 5.0), child(12, 3, 10, "helper.exe", 1.0)]);
        for t in 1..=30 {
            pv.rebuild(&orphans, "", at(t * 1000, true));
            let sh = shape(&pv);
            assert_eq!(sh[..2], strs(&["H:App:0", "x10"])[..], "t={t}: one ghost for the root: {sh:?}");
            assert_eq!(sorted(sh[2..].to_vec()), sorted(strs(&["C11", "C12", "xc10"])), "t={t}: descendants stay under it: {sh:?}");
        }
        // survivors are real, actionable rows; the ghosts are not
        let live: Vec<_> = pv.row_ids.iter().flatten().map(|i| (i.0, i.2)).collect();
        assert_eq!(live, vec![(11, false), (12, false)]);
        pv.sel = Some((11, 2, false));
        assert_eq!(pv.selected_proc(&orphans).map(|p| p.pid), Some(11));
        pv.sel = Some((10, 1, true));
        assert!(pv.selected_proc(&orphans).is_none(), "the exited root is no target");
        // pointer leaves: after the grace and the timer the list is rebuilt from live data
        thaw(&mut pv, &orphans, 31_000, 40_000);
        let sh = shape(&pv);
        assert!(!sh.iter().any(|x| x.starts_with('x') || x.starts_with('C')), "no ghost and no stale block: {sh:?}");
        let live: Vec<_> = pv.row_ids.iter().flatten().map(|i| (i.0, i.2)).collect();
        assert_eq!(sorted(live.iter().map(|g| format!("{}:{}", g.0, g.1)).collect()), strs(&["11:true", "12:true"]));
    }

    #[test]
    fn section_change_keeps_the_held_slot_without_showing_it_twice() {
        let mut pv = ProcView::new();
        pv.rebuild(&snap(vec![proc(21, 10, "a.exe", 5.0), proc(22, 20, "b.exe", 1.0)]), "", at(0, true));
        assert_eq!(shape(&pv), strs(&["H:App:2", "G21", "G22"]));
        // a.exe loses its window: it is a background process now
        let mut a = proc(21, 10, "a.exe", 5.0);
        a.windowed = false;
        let moved = snap(vec![a, proc(22, 20, "b.exe", 1.0)]);
        for t in 1..=10 {
            pv.rebuild(&moved, "", at(t * 1000, true));
            assert_eq!(shape(&pv), strs(&["H:App:2", "G21", "G22"]), "t={t}: frozen, still listed, still live");
        }
        assert_eq!(pv.id_at(1), Some((21, 10, true)), "still the real, actionable process");
        // thaw: it moves, exactly once
        thaw(&mut pv, &moved, 11_000, 20_000);
        assert_eq!(shape(&pv), strs(&["H:App:1", "G22", "H:Background:1", "G21"]));
        assert_eq!(pv.row_ids.iter().flatten().filter(|r| r.0 == 21).count(), 1);
    }

    #[test]
    fn section_change_under_an_open_menu_holds_too() {
        let mut pv = ProcView::new();
        pv.rebuild(&snap(vec![proc(21, 10, "a.exe", 5.0), proc(22, 20, "b.exe", 1.0)]), "", held(1, 0));
        let mut a = proc(21, 10, "a.exe", 5.0);
        a.windowed = false;
        let moved = snap(vec![a, proc(22, 20, "b.exe", 1.0)]);
        for t in 1..=20 {
            pv.rebuild(&moved, "", held(1, t * 1000));
            assert_eq!(shape(&pv), strs(&["H:App:2", "G21", "G22"]), "t={t}");
        }
    }

    #[test]
    fn explicit_filter_or_sort_still_applies_at_once_to_a_held_tree() {
        let mk = || {
            let mut pv = ProcView::new();
            pv.expanded.insert((10, 1));
            pv.rebuild(&app_with_two_children(5.0, 1.0), "", at(0, true));
            let one = snap(vec![proc(10, 1, "app.exe", 0.0), proc(20, 2, "other.exe", 3.0)]);
            pv.rebuild(&one, "", at(1000, true));
            assert_eq!(pv.gone.len(), 2, "held first: {:?}", shape(&pv));
            (pv, one)
        };
        let (mut pv, one) = mk();
        pv.rebuild(&one, "other", at(1500, true));
        assert_eq!(shape(&pv), strs(&["H:App:1", "G20"]), "filter applies under the pointer");
        let (mut pv, one) = mk();
        let name = col(&pv, Col::Name);
        pv.sort_by(name);
        pv.rebuild(&one, "", at(1500, true));
        assert!(pv.gone.is_empty(), "sort applies under the pointer: {:?}", shape(&pv));
    }

    #[test]
    fn tree_expansion_never_adopts_a_replacement_roots_children() {
        let p = |pid, ppid, create| Proc { ppid, ..proc(pid, create, "x.exe", 0.0) };
        let pids = |v: Vec<(u32, i64)>| {
            let mut x: Vec<u32> = v.iter().map(|k| k.0).collect();
            x.sort();
            x
        };
        // picked root (10, create 100) exited; PID 10 now belongs to a NEW process (create 500)
        // with its own child 11, and 12 is a leftover that names ppid 10 too
        let snapshot = vec![p(10, 0, 500), p(11, 10, 600), p(12, 10, 150)];
        assert_eq!(with_descendants(&snapshot, vec![(10, 100)]), vec![(10, 100)], "root not in the snapshot: nothing is added");
        // the exact root present: its descendants, recursively, only the younger ones
        let snapshot = vec![p(10, 0, 100), p(11, 10, 150), p(12, 11, 160), p(13, 10, 90), p(14, 99, 170)];
        assert_eq!(pids(with_descendants(&snapshot, vec![(10, 100)])), vec![10, 11, 12]);
        // an expanded child whose PID was reused does not pull in the reuser's children
        let snapshot = vec![p(10, 0, 100), p(11, 1, 900), p(13, 11, 950)];
        let got = with_descendants(&snapshot, vec![(10, 100), (11, 150)]);
        assert_eq!(got, vec![(10, 100), (11, 150)], "(11,150) is not in the snapshot: 13 is not adopted");
    }

    // ---------- menu target ----------

    fn target_view(procs: Vec<Proc>) -> (ProcView, Arc<Snapshot>) {
        let mut pv = ProcView::new();
        let s = snap(procs);
        pv.rebuild(&s, "", at(0, false));
        (pv, s)
    }

    #[test]
    fn processes_menu_target_is_the_selected_row_by_identity() {
        let (mut pv, s) = target_view(vec![proc(10, 100, "app.exe", 5.0), child(11, 110, 10, "app.exe", 1.0), child(12, 120, 10, "app.exe", 1.0), proc(20, 200, "solo.exe", 1.0)]);
        // the group row: every member, the group named, its root the representative
        pv.sel = Some((10, 100, true));
        let t = pv.menu_target(&s).unwrap();
        assert_eq!((t.origin, t.group, t.members.len()), (Origin::Processes, true, 3));
        assert_eq!(t.rep.as_ref().map(|r| (r.pid, r.create)), Some((10, 100)));
        // one child: just that process
        pv.sel = Some((11, 110, false));
        let t = pv.menu_target(&s).unwrap();
        assert_eq!((t.group, t.members.iter().map(|m| (m.0, m.1)).collect::<Vec<_>>()), (false, vec![(11, 110)]));
        assert_eq!(t.label, "app.exe");
        // nothing selected: no menu
        pv.sel = None;
        assert!(pv.menu_target(&s).is_none());
    }

    #[test]
    fn processes_menu_target_survives_a_new_snapshot_only_by_identity() {
        let (mut pv, s) = target_view(vec![proc(10, 100, "app.exe", 5.0), child(11, 110, 10, "app.exe", 1.0), proc(20, 200, "solo.exe", 1.0)]);
        pv.sel = Some((10, 100, true));
        // the child exits: the group target keeps the root only
        let later = snap(vec![proc(10, 100, "app.exe", 5.0), proc(20, 200, "solo.exe", 1.0)]);
        let t = pv.menu_target(&later).unwrap();
        assert_eq!(t.members.iter().map(|m| (m.0, m.1)).collect::<Vec<_>>(), vec![(10, 100)]);
        // the whole row exited
        let gone = snap(vec![proc(20, 200, "solo.exe", 1.0)]);
        assert!(pv.menu_target(&gone).is_none());
        // pid 20 reused by another process: the picked one is not there
        pv.sel = Some((20, 200, false));
        let reused = snap(vec![proc(10, 100, "app.exe", 5.0), proc(20, 999, "other.exe", 1.0)]);
        assert!(pv.menu_target(&reused).is_none());
        assert!(pv.menu_target(&s).is_some());
    }

    #[test]
    fn command_line_is_optional_hidden_on_a_fresh_start_and_a_choice_sticks() {
        let mut pv = ProcView::new();
        let ci = col(&pv, Col::CmdLine);
        assert!(!pv.cols[ci].on, "fresh settings: no Command line column");
        for c in [Col::Name, Col::Status, Col::Pid, Col::Cpu, Col::Mem, Col::Disk, Col::Gpu] {
            assert!(pv.cols[col(&pv, c)].on, "{c:?} stays on by default");
        }
        // the user turns it on, resizes it: rebuilds (every sample) keep that
        pv.toggle_col(ci);
        pv.cols[ci].w = 333.0;
        for t in [0u64, 1000, 6000] {
            pv.rebuild(&snap(vec![proc(1, 10, "a.exe", 1.0)]), "", at(t, false));
            assert!(pv.cols[ci].on && pv.cols[ci].w == 333.0, "t={t}");
        }
        pv.toggle_col(ci);
        pv.rebuild(&snap(vec![proc(1, 10, "a.exe", 1.0)]), "", at(7000, false));
        assert!(!pv.cols[col(&pv, Col::CmdLine)].on, "and it can be hidden again");
    }

    #[test]
    fn a_cell_is_found_only_inside_the_visible_list() {
        let mut pv = ProcView::new();
        pv.rebuild(&snap(vec![proc(1, 10, "a.exe", 9.0), proc(2, 20, "b.exe", 1.0)]), "", at(0, false));
        pv.list = Rect::new(100.0, 80.0, 600.0, 66.0); // three rows high
        pv.cols_x = vec![(100.0, 300.0, 0), (300.0, 380.0, 2)];
        let first = pv.row_ids.iter().position(|r| r.is_some()).unwrap();
        let y = |ri: usize| 80.0 + ri as f32 * ROW_H + 5.0 - pv.scroll;
        assert_eq!(pv.cell_at(150.0, y(first)), Some((first, 0)));
        assert_eq!(pv.cell_at(350.0, y(first)), Some((first, 2)));
        assert_eq!(pv.cell_at(399.0, y(first)), None, "no shown column there");
        assert_eq!(pv.cell_at(150.0, 79.0), None, "above the list (header)");
        assert_eq!(pv.cell_at(150.0, 80.0 + 66.0 + 1.0), None, "below the list");
        assert_eq!(pv.cell_at(701.0, y(first)), None, "right of the list");
        // scrolled: the row under a point changes, a row scrolled out of the list has no cell
        pv.scroll = ROW_H;
        assert_eq!(pv.cell_at(150.0, 85.0).map(|c| c.0), Some(1));
        let r = pv.cell_rect(first, 0).unwrap();
        assert_eq!((r.x, r.w, r.h), (100.0, 200.0, ROW_H));
        assert_eq!(r.y, 80.0 + first as f32 * ROW_H - ROW_H);
    }

    fn windowed_child(pid: u32, create: i64, ppid: u32, name: &str) -> Proc {
        Proc { ppid, ..proc(pid, create, name, 0.0) }
    }

    fn group_of(groups: &[Group], pid: u32) -> &Group {
        groups.iter().find(|g| g.key.0 == pid).unwrap_or_else(|| panic!("no group for pid {pid}: {:?}", groups.iter().map(|g| (g.key, g.name.clone())).collect::<Vec<_>>()))
    }

    #[test]
    fn our_own_image_names_are_recognised() {
        for n in ["hayate.exe", "HAYATE.EXE", "Hayate", "hayate-tm.exe", "hayate-tm.old.exe", "hayate.new.exe", "hayate-1234.exe", "hayate-multi.exe", "blink.exe", "blink-tm.exe", "blink.old.exe", "blink-77.exe"] {
            assert!(is_own_image(n), "{n}");
        }
        for n in ["notepad.exe", "hayatex.exe", "hayate-ish.exe", "blinkfeed.exe", "explorer.exe", ""] {
            assert!(!is_own_image(n), "{n}");
        }
    }

    /// The screenshot case: Hayate is the parent of everything it starts at
    /// sign-in. Its row must count Hayate only, and each started program keeps
    /// its own helpers.
    #[test]
    fn programs_hayate_started_are_their_own_groups() {
        let s = snap(vec![
            proc(100, 10, "hayate.exe", 0.0),
            windowed_child(200, 20, 100, "deckviz.exe"), // started app with a window
            child(300, 30, 200, "conhost.exe", 0.0),     // its console
            child(400, 40, 100, "node.exe", 0.0),        // started app without a window
            child(500, 50, 400, "conhost.exe", 0.0),     // its console
        ]);
        let g = build_groups(&s);
        assert_eq!(group_of(&g, 100).members.len(), 1, "Hayate's own row is Hayate only");
        assert_eq!(group_of(&g, 200).members.len(), 2, "the started app keeps its console");
        let node = group_of(&g, 400);
        assert_eq!(node.members.len(), 2, "the console hangs off the program, not off Hayate");
        assert_eq!(node.kind, Kind::Background);
        assert_eq!(g.len(), 3);
    }

    /// A second copy of us (helper, launcher, pre-rename blink.exe) is still us.
    #[test]
    fn another_copy_of_our_image_still_joins_us() {
        let s = snap(vec![
            proc(100, 10, "hayate.exe", 0.0),
            child(200, 20, 100, "hayate-tm.exe", 0.0),
            child(300, 30, 200, "conhost.exe", 0.0),
        ]);
        let g = build_groups(&s);
        assert_eq!(group_of(&g, 100).members.len(), 3);
        // and the pre-rename name counts the same way
        let s = snap(vec![proc(100, 10, "blink.exe", 0.0), child(200, 20, 100, "node.exe", 0.0)]);
        let g = build_groups(&s);
        assert_eq!(group_of(&g, 100).members.len(), 1);
        assert_eq!(group_of(&g, 200).members.len(), 1);
    }

    /// Everything else keeps Task Manager's grouping.
    #[test]
    fn unrelated_trees_are_grouped_as_before() {
        let s = snap(vec![
            proc(10, 10, "explorer.exe", 0.0),
            child(20, 20, 10, "svchost.exe", 0.0),
            child(30, 30, 20, "service.exe", 0.0), // non-windowed under non-windowed: own row, as before
            windowed_child(40, 40, 0, "game.exe"),
            child(50, 50, 40, "helper.exe", 0.0), // joins the app with the window
        ]);
        let g = build_groups(&s);
        assert_eq!(group_of(&g, 20).members.len(), 1);
        assert_eq!(group_of(&g, 30).members.len(), 1);
        assert_eq!(group_of(&g, 40).members.len(), 2);
    }
}
