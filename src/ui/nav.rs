//! Keyboard list navigation shared by the tabs (Processes keeps its own
//! tree-aware version in procs.rs). Every list remembers the rows of its last
//! paint by stable identity (service name, rule id, event id, ...), never by
//! screen position: a key moves from the selected row as the user saw it, and
//! a destructive button acts on whatever is selected when it is pressed.

use super::{App, Hit, Tab, LN};
use crate::gfx::{Gfx, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Move {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

impl Move {
    pub fn from_vk(vk: u16) -> Option<Move> {
        Some(match vk {
            0x26 => Move::Up,
            0x28 => Move::Down,
            0x21 => Move::PageUp,
            0x22 => Move::PageDown,
            0x24 => Move::Home,
            0x23 => Move::End,
            _ => return None,
        })
    }

    /// Rows moved (negative: up); None for Home / End.
    fn delta(self, page: usize) -> Option<isize> {
        let p = page.max(1) as isize;
        match self {
            Move::Up => Some(-1),
            Move::Down => Some(1),
            Move::PageUp => Some(-p),
            Move::PageDown => Some(p),
            Move::Home | Move::End => None,
        }
    }
}

/// Index the selection moves to in a list of `len` rows, clamped at both ends.
/// `cur`: the selected row's index; `gone_at`: where a selected row that is no
/// longer listed was last seen (the rows after it moved up into its place, so
/// Down lands on the row that took its slot). No selection: forward moves
/// start at the first row, backward ones at the last.
pub fn target(len: usize, cur: Option<usize>, gone_at: Option<usize>, m: Move, page: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len as isize - 1;
    let Some(d) = m.delta(page) else {
        return Some(if m == Move::Home { 0 } else { last as usize });
    };
    let i = match (cur, gone_at) {
        (Some(i), _) => i as isize + d,
        (None, Some(a)) => a as isize + if d > 0 { d - 1 } else { d },
        (None, None) => {
            if d > 0 {
                0
            } else {
                last
            }
        }
    };
    Some(i.clamp(0, last) as usize)
}

/// Rows one PageUp / PageDown moves: a view of rows minus one kept for context.
pub fn page_rows(view_h: f32, row_h: f32) -> usize {
    if row_h <= 0.0 {
        return 1;
    }
    ((view_h / row_h).floor().max(0.0) as usize).saturating_sub(1).max(1)
}

/// Scroll offset that shows the span `[top, top + h)` (content coordinates)
/// in a view `view_h` tall, moving as little as possible; a span taller than
/// the view shows its top.
pub fn reveal(scroll: f32, top: f32, h: f32, view_h: f32) -> f32 {
    if top < scroll {
        top
    } else if top + h > scroll + view_h {
        (top + h - view_h).min(top)
    } else {
        scroll
    }
}

// ---- scrollbars ------------------------------------------------------------------------------
//
// One look and one drag rule for every scrolling view that has no scrollbar of its own (Processes
// keeps its own; History, Services, Startup, Rules, Watch, Vitals and Settings use these). A view
// calls `gutter` to learn how much width to give up when its content overflows (0 when it fits, so a
// layout that does not overflow is unchanged), lays its rows out that much narrower, and after
// drawing calls `paint_bar` with the view rect, the content height and its scroll offset. That draws
// the thumb and registers `Hit::Bar(id)`; `App` turns a press / drag on it into a new scroll offset
// (`Bars`) and writes it back through `App::bar_scroll`. The view still clamps its own scroll when it
// paints, so the wheel, the arrow keys and a drag share one clamp.

/// Width taken from a view's rows while its bar shows: the 6 px thumb, 2 px from the edge, plus air.
pub const GUTTER: f32 = 10.0;
/// Visible thumb width.
pub const BAR_W: f32 = 6.0;
/// Shortest thumb, so it stays grabbable in a very long list.
pub const THUMB_MIN: f32 = 24.0;
/// The track starts / ends this far inside the view's top / bottom.
const TRACK_INSET: f32 = 2.0;
const THUMB_HOT: u32 = 0x3a414d;

/// Ids of the scroll bars (`Hit::Bar(id)`).
pub const BAR_HISTORY: u8 = 1;
pub const BAR_SERVICES: u8 = 2;
/// Startup: the Windows entries list and the Hayate list share one scroll offset
pub const BAR_STARTUP: u8 = 3;
pub const BAR_STARTUP_EDIT: u8 = 4;
pub const BAR_STARTUP_IMPORT: u8 = 5;
pub const BAR_RULES: u8 = 6;
pub const BAR_WATCH: u8 = 7;
pub const BAR_VIT_LIST: u8 = 8;
pub const BAR_VIT_DETAIL: u8 = 9;
pub const BAR_SETTINGS: u8 = 10;

/// Content taller than its view (half a pixel of float noise is not overflow).
pub fn overflows(content_h: f32, view_h: f32) -> bool {
    content_h > view_h + 0.5
}

/// Width a view gives up for its bar: `GUTTER` while the content overflows, else 0.
pub fn gutter(content_h: f32, view_h: f32) -> f32 {
    if overflows(content_h, view_h) {
        GUTTER
    } else {
        0.0
    }
}

/// Thumb `(offset from the track top, length)` on a track `track_h` tall for content `content_h` in a
/// view `view_h` scrolled by `scroll`. The length is the visible share of the track, at least
/// `THUMB_MIN` and at most the track; scroll is clamped, so the thumb never leaves the track.
pub fn thumb(track_h: f32, content_h: f32, view_h: f32, scroll: f32) -> (f32, f32) {
    let track_h = track_h.max(0.0);
    if !overflows(content_h, view_h) || track_h <= 0.0 {
        return (0.0, track_h);
    }
    let len = (view_h / content_h * track_h).clamp(THUMB_MIN.min(track_h), track_h);
    let max = content_h - view_h;
    (scroll.clamp(0.0, max) / max * (track_h - len), len)
}

/// Scroll offset for a thumb whose top is `top` below the track top (clamped to the track).
pub fn scroll_for_top(top: f32, track_h: f32, content_h: f32, view_h: f32) -> f32 {
    let max = (content_h - view_h).max(0.0);
    let (_, len) = thumb(track_h, content_h, view_h, 0.0);
    let room = track_h - len;
    if max <= 0.0 || room <= 0.0 {
        return 0.0;
    }
    (top / room).clamp(0.0, 1.0) * max
}

/// Where a press at `pointer` (below the track top) takes hold of the thumb: where it landed on the
/// thumb, or the thumb's middle when the press is on the bare track (the thumb then jumps under it).
pub fn grab_offset(pointer: f32, thumb_top: f32, thumb_len: f32) -> f32 {
    if pointer >= thumb_top && pointer <= thumb_top + thumb_len {
        pointer - thumb_top
    } else {
        thumb_len / 2.0
    }
}

#[derive(Clone, Copy, Debug)]
struct Geom {
    id: u8,
    /// the track the thumb runs on (the hit area is wider)
    track: Rect,
    content_h: f32,
    view_h: f32,
    scroll: f32,
}

/// The bars of the last paint and the drag in progress (`(bar id, grab offset on the thumb)`).
#[derive(Default)]
pub struct Bars {
    geoms: Vec<Geom>,
    drag: Option<(u8, f32)>,
}

impl Bars {
    /// A new paint starts: the views that draw a bar register it again.
    pub fn begin_paint(&mut self) {
        self.geoms.clear();
    }

    fn register(&mut self, g: Geom) {
        self.geoms.retain(|x| x.id != g.id);
        self.geoms.push(g);
    }

    fn geom(&self, id: u8) -> Option<&Geom> {
        self.geoms.iter().find(|g| g.id == id)
    }

    /// Press at `y` on bar `id`: starts the drag, returns the scroll to jump to (unchanged on the thumb).
    pub fn press(&mut self, id: u8, y: f32) -> Option<f32> {
        let g = *self.geom(id)?;
        let (top, len) = thumb(g.track.h, g.content_h, g.view_h, g.scroll);
        let p = y - g.track.y;
        let grab = grab_offset(p, top, len);
        self.drag = Some((id, grab));
        Some(scroll_for_top(p - grab, g.track.h, g.content_h, g.view_h))
    }

    /// The pointer is at `y` during a drag of bar `id`: the scroll it maps to.
    pub fn drag_to(&self, id: u8, y: f32) -> Option<f32> {
        let (d, grab) = self.drag?;
        if d != id {
            return None;
        }
        let g = self.geom(id)?;
        Some(scroll_for_top(y - g.track.y - grab, g.track.h, g.content_h, g.view_h))
    }

    pub fn end_drag(&mut self) {
        self.drag = None;
    }
}

/// Draw bar `id` on the right edge of `view` (the viewport, gutter included) when `content_h`
/// overflows it, and register its hit. Call after the content is drawn and unclipped; the hit goes
/// last so it wins over the rows under it.
pub fn paint_bar(app: &mut App, g: &mut Gfx, id: u8, view: Rect, content_h: f32, scroll: f32) {
    if !overflows(content_h, view.h) || view.h <= 2.0 * TRACK_INSET + 4.0 {
        return;
    }
    let track = Rect::new(view.r() - 8.0, view.y + TRACK_INSET, BAR_W, view.h - 2.0 * TRACK_INSET);
    let (top, len) = thumb(track.h, content_h, view.h, scroll);
    let hot = app.hover == Some(Hit::Bar(id)) || app.down == Some(Hit::Bar(id));
    g.round(Rect::new(track.x, track.y + top, track.w, len), 3.0, if hot { THUMB_HOT } else { LN });
    app.hits.push((Rect::new(view.r() - GUTTER, track.y, GUTTER, track.h), Hit::Bar(id)));
    app.bars.register(Geom { id, track, content_h, view_h: view.h, scroll });
}

/// Left / Right switch the rail tab only where the view has no use for them:
/// Processes expands and collapses its tree with them.
pub fn arrows_switch_tabs(tab: Tab) -> bool {
    tab != Tab::Processes
}

/// Rail index Left / Right goes to from `cur` of `n` tabs (clamped, no wrap).
pub fn side_tab(cur: usize, n: usize, right: bool) -> usize {
    if n == 0 {
        return 0;
    }
    if right {
        (cur + 1).min(n - 1)
    } else {
        cur.saturating_sub(1)
    }
}

/// The rows of a list's last paint, in display order, by a key that resolves
/// them (an identity, or an index into the data the paint drew from).
pub struct ListNav<K> {
    keys: Vec<K>,
    /// where the selection was last seen (paint or key), for a selection that disappears
    last_at: Option<usize>,
    /// rows per PageUp / PageDown
    pub page: usize,
    /// a key moved the selection: the next paint scrolls it into view
    pub reveal: bool,
}

impl<K> Default for ListNav<K> {
    fn default() -> Self {
        ListNav { keys: Vec::new(), last_at: None, page: 1, reveal: false }
    }
}

impl<K: Clone> ListNav<K> {
    /// Start recording a paint's rows (keeps the allocation).
    pub fn begin(&mut self, page: usize) {
        self.keys.clear();
        self.page = page.max(1);
    }

    pub fn push(&mut self, k: K, selected: bool) {
        if selected {
            self.last_at = Some(self.keys.len());
        }
        self.keys.push(k);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// The row a key moves the selection to. `has_sel`: something is selected
    /// (it may no longer be listed); `is_sel` finds it among the keys.
    pub fn step(&mut self, has_sel: bool, is_sel: impl Fn(&K) -> bool, m: Move) -> Option<K> {
        let cur = if has_sel { self.keys.iter().position(is_sel) } else { None };
        let gone = if has_sel && cur.is_none() { self.last_at } else { None };
        let t = target(self.keys.len(), cur, gone, m, self.page)?;
        self.last_at = Some(t);
        self.reveal = true;
        Some(self.keys[t].clone())
    }

    /// Take the pending scroll-into-view request.
    pub fn take_reveal(&mut self) -> bool {
        std::mem::take(&mut self.reveal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_at_both_ends() {
        assert_eq!(target(5, Some(4), None, Move::Down, 3), Some(4));
        assert_eq!(target(5, Some(0), None, Move::Up, 3), Some(0));
        assert_eq!(target(5, Some(2), None, Move::Down, 3), Some(3));
        assert_eq!(target(5, Some(2), None, Move::Up, 3), Some(1));
        assert_eq!(target(1, Some(0), None, Move::Down, 3), Some(0));
    }

    #[test]
    fn paging_and_home_end() {
        assert_eq!(target(100, Some(10), None, Move::PageDown, 20), Some(30));
        assert_eq!(target(100, Some(10), None, Move::PageUp, 20), Some(0));
        assert_eq!(target(100, Some(95), None, Move::PageDown, 20), Some(99));
        assert_eq!(target(100, Some(50), None, Move::Home, 20), Some(0));
        assert_eq!(target(100, Some(50), None, Move::End, 20), Some(99));
        // a zero page still moves one row
        assert_eq!(target(10, Some(3), None, Move::PageDown, 0), Some(4));
        assert_eq!(page_rows(300.0, 30.0), 9);
        assert_eq!(page_rows(20.0, 30.0), 1);
        assert_eq!(page_rows(300.0, 0.0), 1);
    }

    #[test]
    fn empty_list_and_no_selection() {
        for m in [Move::Up, Move::Down, Move::PageUp, Move::PageDown, Move::Home, Move::End] {
            assert_eq!(target(0, None, None, m, 5), None);
            assert_eq!(target(0, Some(3), Some(2), m, 5), None);
        }
        assert_eq!(target(4, None, None, Move::Down, 2), Some(0));
        assert_eq!(target(4, None, None, Move::PageDown, 2), Some(0));
        assert_eq!(target(4, None, None, Move::Up, 2), Some(3));
        let mut n: ListNav<u32> = ListNav::default();
        n.begin(5);
        assert_eq!(n.step(false, |_| false, Move::Down), None);
        assert!(!n.reveal, "nothing to move to: no scroll request");
    }

    #[test]
    fn selection_follows_identity_after_reorder() {
        let mut n: ListNav<&str> = ListNav::default();
        n.begin(10);
        for k in ["a", "b", "c", "d"] {
            n.push(k, k == "b");
        }
        // the list re-sorted between paints: "b" is now third
        n.begin(10);
        for k in ["d", "c", "b", "a"] {
            n.push(k, k == "b");
        }
        assert_eq!(n.step(true, |k| *k == "b", Move::Down), Some("a"));
        assert!(n.take_reveal());
        assert!(!n.take_reveal(), "taken once");
        assert_eq!(n.step(true, |k| *k == "b", Move::Up), Some("c"));
    }

    #[test]
    fn selection_removed_lands_on_neighbour() {
        let mut n: ListNav<&str> = ListNav::default();
        n.begin(10);
        for k in ["a", "b", "c", "d"] {
            n.push(k, k == "c");
        }
        // "c" exited; "d" moved up into its slot
        n.begin(10);
        for k in ["a", "b", "d"] {
            n.push(k, false);
        }
        assert_eq!(n.len(), 3);
        assert_eq!(n.step(true, |k| *k == "c", Move::Down), Some("d"));
        // from a fresh gone selection Up goes to the row above its old slot
        n.begin(10);
        for k in ["a", "b", "c", "d"] {
            n.push(k, k == "c");
        }
        n.begin(10);
        for k in ["a", "b", "d"] {
            n.push(k, false);
        }
        assert_eq!(n.step(true, |k| *k == "c", Move::Up), Some("b"));
        // the last row removed: clamps to the new last
        n.begin(10);
        for k in ["a", "b"] {
            n.push(k, k == "b");
        }
        n.begin(10);
        n.push("a", false);
        assert_eq!(n.step(true, |k| *k == "b", Move::Down), Some("a"));
    }

    #[test]
    fn reveal_moves_as_little_as_needed() {
        assert_eq!(reveal(100.0, 50.0, 30.0, 200.0), 50.0, "above: align top");
        assert_eq!(reveal(100.0, 290.0, 30.0, 200.0), 120.0, "below: align bottom");
        assert_eq!(reveal(100.0, 150.0, 30.0, 200.0), 100.0, "visible: unchanged");
        assert_eq!(reveal(0.0, 400.0, 500.0, 200.0), 400.0, "taller than the view: its top");
    }

    #[test]
    fn bar_gutter_only_when_content_overflows() {
        assert_eq!(gutter(300.0, 300.0), 0.0, "exact fit: no gutter");
        assert_eq!(gutter(300.2, 300.0), 0.0, "float noise is not overflow");
        assert_eq!(gutter(301.0, 300.0), GUTTER);
        assert_eq!(gutter(0.0, 300.0), 0.0);
        assert!(!overflows(100.0, 300.0));
    }

    #[test]
    fn bar_thumb_size_and_position() {
        // half the content visible: half the track
        assert_eq!(thumb(200.0, 400.0, 200.0, 0.0), (0.0, 100.0));
        // at the end the thumb touches the track bottom
        assert_eq!(thumb(200.0, 400.0, 200.0, 200.0), (100.0, 100.0));
        assert_eq!(thumb(200.0, 400.0, 200.0, 100.0), (50.0, 100.0));
        // scroll past either end is clamped onto the track
        assert_eq!(thumb(200.0, 400.0, 200.0, 9999.0).0, 100.0);
        assert_eq!(thumb(200.0, 400.0, 200.0, -50.0).0, 0.0);
        // a huge list: the thumb keeps its minimum length and still reaches the bottom
        let (t, l) = thumb(200.0, 100_000.0, 200.0, 99_800.0);
        assert_eq!(l, THUMB_MIN);
        assert_eq!(t + l, 200.0);
        // a track shorter than the minimum: the thumb is the track, never longer
        assert_eq!(thumb(10.0, 1000.0, 100.0, 0.0), (0.0, 10.0));
        // no overflow: the thumb is the whole track (and is not drawn)
        assert_eq!(thumb(200.0, 100.0, 200.0, 0.0), (0.0, 200.0));
    }

    #[test]
    fn bar_drag_maps_pointer_to_scroll_and_clamps() {
        // 400 content in a 200 view on a 200 track: a 100 thumb with 100 px of room for 200 px of scroll
        assert_eq!(scroll_for_top(0.0, 200.0, 400.0, 200.0), 0.0);
        assert_eq!(scroll_for_top(50.0, 200.0, 400.0, 200.0), 100.0);
        assert_eq!(scroll_for_top(100.0, 200.0, 400.0, 200.0), 200.0);
        assert_eq!(scroll_for_top(-30.0, 200.0, 400.0, 200.0), 0.0, "above the track: top");
        assert_eq!(scroll_for_top(500.0, 200.0, 400.0, 200.0), 200.0, "below the track: end");
        // nothing to scroll
        assert_eq!(scroll_for_top(40.0, 200.0, 100.0, 200.0), 0.0);
        // a press on the thumb keeps its grab point; on the bare track the thumb centres under the pointer
        assert_eq!(grab_offset(30.0, 20.0, 100.0), 10.0);
        assert_eq!(grab_offset(150.0, 20.0, 100.0), 50.0);
        assert_eq!(grab_offset(5.0, 20.0, 100.0), 50.0);
    }

    #[test]
    fn bar_press_and_drag_round_trip() {
        let mut b = Bars::default();
        b.register(Geom { id: BAR_RULES, track: Rect::new(0.0, 10.0, 6.0, 200.0), content_h: 400.0, view_h: 200.0, scroll: 40.0 });
        // the thumb sits at 20..120 (track-relative): a press on it changes nothing yet
        assert_eq!(b.press(BAR_RULES, 10.0 + 60.0), Some(40.0));
        // 25 px down moves the scroll by 25 * (200 / 100) = 50
        assert_eq!(b.drag_to(BAR_RULES, 10.0 + 85.0), Some(90.0));
        // far past either end clamps
        assert_eq!(b.drag_to(BAR_RULES, 10_000.0), Some(200.0));
        assert_eq!(b.drag_to(BAR_RULES, -10_000.0), Some(0.0));
        // another bar's drag is not this one's
        assert_eq!(b.drag_to(BAR_WATCH, 10.0 + 85.0), None);
        b.end_drag();
        assert_eq!(b.drag_to(BAR_RULES, 10.0 + 85.0), None, "no drag, no scroll");
        // a click on the bare track below the thumb centres the thumb under the pointer (then clamps)
        assert_eq!(b.press(BAR_RULES, 10.0 + 180.0), Some(200.0));
        b.end_drag();
        // ... and above it: thumb middle at 5 -> top clamps to 0
        assert_eq!(b.press(BAR_RULES, 10.0 + 5.0), Some(0.0));
        b.end_drag();
        // unknown bar
        assert_eq!(b.press(BAR_STARTUP, 0.0), None);
        // a repaint forgets the geometry until the views register it again
        b.begin_paint();
        assert_eq!(b.press(BAR_RULES, 0.0), None);
    }

    #[test]
    fn left_right_switch_tabs_except_processes_and_clamp() {
        assert!(!arrows_switch_tabs(Tab::Processes));
        for t in [Tab::Vitals, Tab::History, Tab::Startup, Tab::Services, Tab::Rules, Tab::Watch] {
            assert!(arrows_switch_tabs(t), "{t:?}");
        }
        assert_eq!(side_tab(0, 7, false), 0, "clamp left");
        assert_eq!(side_tab(6, 7, true), 6, "clamp right");
        assert_eq!(side_tab(3, 7, true), 4);
        assert_eq!(side_tab(3, 7, false), 2);
        assert_eq!(side_tab(0, 0, true), 0);
    }

    #[test]
    fn vk_mapping() {
        assert_eq!(Move::from_vk(0x26), Some(Move::Up));
        assert_eq!(Move::from_vk(0x28), Some(Move::Down));
        assert_eq!(Move::from_vk(0x21), Some(Move::PageUp));
        assert_eq!(Move::from_vk(0x22), Some(Move::PageDown));
        assert_eq!(Move::from_vk(0x24), Some(Move::Home));
        assert_eq!(Move::from_vk(0x23), Some(Move::End));
        assert_eq!(Move::from_vk(0x25), None, "Left is not a list move");
        assert_eq!(Move::from_vk(0x0D), None);
    }
}
