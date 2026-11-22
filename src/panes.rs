//! A workspace's columns (Bolt's `EditorColumns`, the group actions of
//! `editorStore` and `services/panes/paneActions.ts`): groups side by
//! side, each with its tabs (terminals and files, by id) and one of them
//! shown; one group active. Pure, so the rules are tested without a window.

use std::cell::Cell;

#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub id: u64,
    /// Tab ids (terminals and files), in order.
    pub tabs: Vec<u64>,
    /// The tab shown.
    pub active: usize,
    /// The column's share of the row (`columnWidthsAtom`); 0 until the user
    /// or an action sizes it, and the unsized columns share what is left.
    pub width: f32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Columns {
    pub groups: Vec<Group>,
    pub active_group: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAction {
    SplitLeft,
    SplitRight,
    MoveLeft,
    MoveRight,
    FocusLeft,
    FocusRight,
    ReorderLeft,
    ReorderRight,
    Rotate,
    Equalize,
    Maximize,
}

/// The pane overlay's rows: its key, label and action (`PaneOverlay`'s
/// `ROWS`).
pub const ROWS: &[&[(&str, &str, PaneAction)]] = &[
    &[("E", "Split left", PaneAction::SplitLeft), ("R", "Split right", PaneAction::SplitRight)],
    &[("D", "Move tab left", PaneAction::MoveLeft), ("F", "Move tab right", PaneAction::MoveRight)],
    &[("C", "Focus column ←", PaneAction::FocusLeft), ("V", "Focus column →", PaneAction::FocusRight)],
    &[("Q", "Reorder tab ←", PaneAction::ReorderLeft), ("W", "Reorder tab →", PaneAction::ReorderRight)],
    &[("A", "Rotate columns", PaneAction::Rotate), ("S", "Equalize widths", PaneAction::Equalize)],
    &[("Z", "Cycle column width", PaneAction::Maximize)],
];

/// `MAXIMIZE_STEPS`: the active column's share at each step of the cycle.
const MAXIMIZE_STEPS: [f32; 2] = [0.75, 0.3];

/// The widths a maximize cycle replaced and where it is (`maximizeSnapshot`).
#[derive(Debug, Clone, PartialEq)]
pub struct MaximizeSnapshot {
    ids: Vec<u64>,
    widths: Vec<f32>,
    step: usize,
}

fn new_group_id() -> u64 {
    thread_local!(static NEXT: Cell<u64> = const { Cell::new(1) });
    NEXT.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    })
}

fn group(tabs: Vec<u64>, active: usize) -> Group {
    Group { id: new_group_id(), tabs, active, width: 0.0 }
}

impl Columns {
    fn active_index(&self) -> Option<usize> {
        self.groups.iter().position(|g| g.id == self.active_group)
    }

    /// The active group and its shown tab (`cursor`).
    fn cursor(&self) -> Option<(usize, usize, usize)> {
        let gi = self.active_index()?;
        let g = &self.groups[gi];
        (g.active < g.tabs.len()).then_some((gi, g.active, g.tabs.len()))
    }

    /// The tab the active group shows.
    pub fn shown(&self) -> Option<u64> {
        let (gi, ti, _) = self.cursor()?;
        Some(self.groups[gi].tabs[ti])
    }

    /// The tab group `id` shows.
    pub fn shown_in(&self, id: u64) -> Option<u64> {
        let g = self.groups.iter().find(|g| g.id == id)?;
        g.tabs.get(g.active).copied()
    }

    /// Adds tab `term` to the active group, or to a new column after it
    /// (`ensureGroup`, `openInNewColumn`), and shows it.
    pub fn add(&mut self, term: u64, new_column: bool) {
        match self.active_index() {
            Some(gi) if !new_column => {
                let g = &mut self.groups[gi];
                g.tabs.push(term);
                g.active = g.tabs.len() - 1;
            }
            at => {
                let g = group(vec![term], 0);
                self.active_group = g.id;
                self.groups.insert(at.map_or(self.groups.len(), |i| i + 1), g);
            }
        }
    }

    /// Shows tab `term` and makes its group active.
    pub fn show(&mut self, term: u64) {
        for g in &mut self.groups {
            if let Some(i) = g.tabs.iter().position(|t| *t == term) {
                g.active = i;
                self.active_group = g.id;
                return;
            }
        }
    }

    /// Takes tab `term` out (`closeTabAtom`): the tab before it shows,
    /// and an emptied group goes, the one before it taking over.
    pub fn remove(&mut self, term: u64) {
        let Some(gi) = self.groups.iter().position(|g| g.tabs.contains(&term)) else { return };
        let g = &mut self.groups[gi];
        let index = g.tabs.iter().position(|t| *t == term).expect("found above");
        g.tabs.remove(index);
        if g.tabs.is_empty() {
            let gone = self.groups.remove(gi).id;
            if self.active_group == gone {
                self.active_group = self.groups.get(gi.saturating_sub(1)).map_or(0, |g| g.id);
            }
            return;
        }
        if g.active == index {
            g.active = index.saturating_sub(1);
        } else if g.active > index {
            g.active -= 1;
        }
    }

    fn adjacent_group(&self, dir: isize) -> Option<usize> {
        let (gi, ..) = self.cursor()?;
        let next = gi as isize + dir;
        (next >= 0 && (next as usize) < self.groups.len()).then_some(next as usize)
    }

    fn adjacent_tab(&self, dir: isize) -> Option<usize> {
        let (_, ti, count) = self.cursor()?;
        let next = ti as isize + dir;
        (next >= 0 && (next as usize) < count).then_some(next as usize)
    }

    pub fn is_available(&self, action: PaneAction) -> bool {
        let Some((_, _, count)) = self.cursor() else { return false };
        match action {
            PaneAction::SplitLeft | PaneAction::SplitRight => count > 1,
            PaneAction::MoveLeft | PaneAction::FocusLeft => self.adjacent_group(-1).is_some(),
            PaneAction::MoveRight | PaneAction::FocusRight => self.adjacent_group(1).is_some(),
            PaneAction::ReorderLeft => self.adjacent_tab(-1).is_some(),
            PaneAction::ReorderRight => self.adjacent_tab(1).is_some(),
            PaneAction::Rotate | PaneAction::Equalize | PaneAction::Maximize => self.groups.len() > 1,
        }
    }

    /// Carries out `action` (`paneActionAtom`); `maximize` is the cycle's
    /// state, kept between presses.
    pub fn apply(&mut self, action: PaneAction, maximize: &mut Option<MaximizeSnapshot>) {
        if !self.is_available(action) {
            return;
        }
        let (gi, ti, _) = self.cursor().expect("available");
        match action {
            PaneAction::SplitLeft | PaneAction::SplitRight => {
                // `splitActiveTab`: the shown tab into a new column beside.
                let g = &mut self.groups[gi];
                let term = g.tabs.remove(ti);
                g.active = ti.saturating_sub(1);
                let new = group(vec![term], 0);
                self.active_group = new.id;
                let at = if action == PaneAction::SplitLeft { gi } else { gi + 1 };
                self.groups.insert(at, new);
            }
            PaneAction::MoveLeft | PaneAction::MoveRight => {
                // `moveTabToGroupAtom`: to the end of the neighbour, shown there;
                // an emptied source goes.
                let to = self.adjacent_group(if action == PaneAction::MoveLeft { -1 } else { 1 }).expect("available");
                let to_id = self.groups[to].id;
                let src = &mut self.groups[gi];
                let term = src.tabs.remove(ti);
                let emptied = src.tabs.is_empty();
                if !emptied && src.active >= src.tabs.len() {
                    src.active = src.tabs.len() - 1;
                } else if !emptied && src.active > ti {
                    src.active -= 1;
                }
                let dst = &mut self.groups[to];
                dst.tabs.push(term);
                dst.active = dst.tabs.len() - 1;
                if emptied {
                    self.groups.remove(gi);
                }
                self.active_group = to_id;
            }
            PaneAction::FocusLeft | PaneAction::FocusRight => {
                let to = self.adjacent_group(if action == PaneAction::FocusLeft { -1 } else { 1 }).expect("available");
                self.active_group = self.groups[to].id;
            }
            PaneAction::ReorderLeft | PaneAction::ReorderRight => {
                let to = self.adjacent_tab(if action == PaneAction::ReorderLeft { -1 } else { 1 }).expect("available");
                let g = &mut self.groups[gi];
                let term = g.tabs.remove(ti);
                g.tabs.insert(to, term);
                g.active = to;
            }
            PaneAction::Rotate => {
                let last = self.groups.pop().expect("available");
                self.groups.insert(0, last);
            }
            PaneAction::Equalize => {
                *maximize = None;
                let share = 1.0 / self.groups.len() as f32;
                for g in &mut self.groups {
                    g.width = share;
                }
            }
            PaneAction::Maximize => {
                let ids: Vec<u64> = self.groups.iter().map(|g| g.id).collect();
                let snapshot = maximize.take().filter(|s| s.ids == ids);
                let step = snapshot.as_ref().map_or(0, |s| s.step + 1);
                if step >= MAXIMIZE_STEPS.len() {
                    // The cycle's end restores the widths it replaced.
                    let widths = snapshot.expect("a later step has a snapshot").widths;
                    for (g, w) in self.groups.iter_mut().zip(widths) {
                        g.width = w;
                    }
                    return;
                }
                let widths = snapshot.map_or_else(|| self.groups.iter().map(|g| g.width).collect(), |s| s.widths);
                let fraction = MAXIMIZE_STEPS[step];
                let rest = (1.0 - fraction) / (self.groups.len() - 1) as f32;
                let active = self.active_group;
                for g in &mut self.groups {
                    g.width = if g.id == active { fraction } else { rest };
                }
                *maximize = Some(MaximizeSnapshot { ids, widths, step });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(groups: &[&[u64]], active: usize) -> Columns {
        let mut c = Columns::default();
        for (i, terms) in groups.iter().enumerate() {
            let g = group(terms.to_vec(), terms.len().saturating_sub(1));
            if i == active {
                c.active_group = g.id;
            }
            c.groups.push(g);
        }
        c
    }

    fn layout(c: &Columns) -> Vec<Vec<u64>> {
        c.groups.iter().map(|g| g.tabs.clone()).collect()
    }

    #[test]
    fn split_right_moves_the_shown_tab_into_a_new_column_after() {
        let mut c = cols(&[&[1, 2, 3]], 0);
        c.show(2);
        c.apply(PaneAction::SplitRight, &mut None);
        assert_eq!(layout(&c), [vec![1, 3], vec![2]]);
        assert_eq!(c.shown(), Some(2));
        assert_eq!(c.shown_in(c.groups[0].id), Some(1), "the tab before the moved one shows");
        assert!(!cols(&[&[1]], 0).is_available(PaneAction::SplitRight), "a lone tab cannot split");
    }

    #[test]
    fn moving_the_last_tab_out_removes_its_column() {
        let mut c = cols(&[&[1], &[2, 3]], 0);
        c.apply(PaneAction::MoveRight, &mut None);
        assert_eq!(layout(&c), [vec![2, 3, 1]]);
        assert_eq!(c.shown(), Some(1));
    }

    #[test]
    fn closing_shows_the_previous_tab_and_an_empty_column_hands_focus_left() {
        let mut c = cols(&[&[1, 2], &[3]], 1);
        c.remove(3);
        assert_eq!(layout(&c), [vec![1, 2]]);
        assert_eq!(c.shown(), Some(2));
        c.show(2);
        c.remove(2);
        assert_eq!(c.shown(), Some(1));
    }

    #[test]
    fn maximize_cycles_then_restores() {
        let mut c = cols(&[&[1], &[2], &[3]], 1);
        let mut snap = None;
        c.apply(PaneAction::Maximize, &mut snap);
        assert_eq!(c.groups[1].width, 0.75);
        assert_eq!(c.groups[0].width, 0.125);
        c.apply(PaneAction::Maximize, &mut snap);
        assert_eq!(c.groups[1].width, 0.3);
        c.apply(PaneAction::Maximize, &mut snap);
        assert!(c.groups.iter().all(|g| g.width == 0.0), "back to the widths before the cycle");
    }
}
