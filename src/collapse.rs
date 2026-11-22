//! The strip's collapse state (Bolt's `searchStore` collapse atoms and the
//! ⌘0 cycle, `togglePinnedCollapsedAtom`): Pinned and each group walk the
//! same steps — 0 open, 1 collapsed (rows drawing terminal chips stay),
//! 2 only the group's session card, 3 folded — the header click moving one,
//! ⌘0 moving all but the locked. Pure, so the cycle is tested without a
//! window; saved as Bolt's `workspace-collapse.json`.

use serde::{Deserialize, Serialize};

use crate::settings;

const FILE: &str = "workspace-collapse.json";

/// What the unlocked groups showed before a ⌘0 cycle started, so its last
/// press restores only what the cycle closed (`groupsBeforeCycleAtom`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Before {
    pub collapsed: Vec<String>,
    pub home_only: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Collapse {
    pub pinned_collapsed: bool,
    /// Pinned's step 3: its chipped rows folded too.
    pub pinned_chipped_collapsed: bool,
    /// ⌘0 leaves Pinned as it is; its header still moves it.
    pub pinned_locked: bool,
    /// Every group past step 0.
    pub collapsed_groups: Vec<String>,
    /// Step 3: the collapsed groups whose chipped rows fold too.
    pub chipped_collapsed: Vec<String>,
    /// Step 2: groups showing only their session card.
    pub home_only_groups: Vec<String>,
    /// Groups ⌘0 leaves alone.
    pub locked_groups: Vec<String>,
    pub groups_before_cycle: Option<Before>,
}

/// What the cycle reads from the rest of the shell.
#[derive(Debug, Clone, Default)]
pub struct Strip {
    /// The settings' groups, in order.
    pub groups: Vec<String>,
    /// The groups with a session home (`groupHomeOf`).
    pub homes: Vec<String>,
    /// Some row (a pinned or visible tab, or a home) draws a chip list
    /// (`chippedWorkspacesAtom`): it has terminals.
    pub chipped: bool,
}

/// A group's step, as its header shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Open,
    Collapsed,
    Session,
    Folded,
}

fn without(list: &[String], name: &str) -> Vec<String> {
    list.iter().filter(|g| *g != name).cloned().collect()
}

impl Collapse {
    /// This app's saved state, else the Electron Bolt's, else all open.
    pub fn load() -> Collapse {
        [settings::data_dir(), settings::bolt_data_dir()]
            .iter()
            .find_map(|d| serde_json::from_str(&std::fs::read_to_string(d.join(FILE)).ok()?).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let dir = settings::data_dir();
        let write = std::fs::create_dir_all(&dir).and_then(|()| {
            std::fs::write(dir.join(FILE), serde_json::to_string_pretty(self).unwrap_or_default())
        });
        if let Err(e) = write {
            eprintln!("rivet: could not save the strip's collapse state: {e}");
        }
    }

    pub fn group_step(&self, name: &str) -> Step {
        let has = |list: &[String]| list.iter().any(|g| g == name);
        if !has(&self.collapsed_groups) {
            Step::Open
        } else if has(&self.chipped_collapsed) {
            Step::Folded
        } else if has(&self.home_only_groups) {
            Step::Session
        } else {
            Step::Collapsed
        }
    }

    pub fn is_locked(&self, name: &str) -> bool {
        self.locked_groups.iter().any(|g| g == name)
    }

    /// The groups ⌘0 moves (`cycledGroupsAtom`).
    fn cycled(&self, strip: &Strip) -> Vec<String> {
        strip.groups.iter().filter(|g| !self.is_locked(g)).cloned().collect()
    }

    fn cycled_homes(&self, strip: &Strip) -> Vec<String> {
        self.cycled(strip).into_iter().filter(|g| strip.homes.contains(g)).collect()
    }

    /// The steps ⌘0 walks (`cycleStepsAtom`): 2 only while a cycled group
    /// has a home, 3 only while some row draws chips.
    fn steps(&self, strip: &Strip) -> Vec<u8> {
        let mut steps = vec![0, 1];
        if !self.cycled_homes(strip).is_empty() {
            steps.push(2);
        }
        if strip.chipped {
            steps.push(3);
        }
        steps
    }

    /// The step the cycle is at (`cycleStepAtom`): the least collapsed of
    /// Pinned and the cycled groups, so one press lands them together;
    /// what looks the same at steps 1 and 2 counts as 2 once collapsed;
    /// clamped to a step that exists.
    pub fn cycle_step(&self, strip: &Strip) -> u8 {
        let homes = self.cycled_homes(strip);
        let pin = if self.pinned_collapsed { if self.pinned_chipped_collapsed { 3 } else { 2 } } else { 0 };
        let groups: Vec<u8> = self
            .cycled(strip)
            .iter()
            .map(|g| match self.group_step(g) {
                Step::Folded => 3,
                Step::Session => 2,
                Step::Collapsed if homes.contains(g) => 1,
                Step::Collapsed => 2,
                Step::Open => 0,
            })
            .collect();
        let raw = (!self.pinned_locked).then_some(pin).into_iter().chain(groups.iter().copied()).chain([3]).min().unwrap_or(3);
        let lowest = if !groups.is_empty() || !self.pinned_locked { raw } else { 0 };
        self.steps(strip).into_iter().filter(|s| *s <= lowest).max().unwrap_or(0)
    }

    fn set_pinned_step(&mut self, step: u8) {
        self.pinned_collapsed = step > 0;
        self.pinned_chipped_collapsed = step == 3;
    }

    /// ⌘0 (`togglePinnedCollapsedAtom`). With the strip hidden it only
    /// shows it (`false` back: the caller opens the strip), Pinned opened
    /// unless locked and the groups untouched.
    pub fn cycle(&mut self, strip: &Strip, visible: bool) {
        if !visible {
            if !self.pinned_locked {
                self.set_pinned_step(0);
            }
            return;
        }
        let steps = self.steps(strip);
        let at = steps.iter().position(|s| *s == self.cycle_step(strip)).unwrap_or(0);
        let next = steps[(at + 1) % steps.len()];
        if !self.pinned_locked {
            self.set_pinned_step(next);
        }
        let groups = self.cycled(strip);
        if groups.is_empty() {
            return;
        }
        let locked = self.locked_groups.clone();
        let keep = |list: &[String]| -> Vec<String> { list.iter().filter(|g| locked.contains(g)).cloned().collect() };
        let unlocked = |list: &[String]| -> Vec<String> { list.iter().filter(|g| !locked.contains(g)).cloned().collect() };
        let (collapsed, folded, home_only) = (self.collapsed_groups.clone(), self.chipped_collapsed.clone(), self.home_only_groups.clone());
        let all_collapsed = [keep(&collapsed), groups.clone()].concat();
        match next {
            1 => {
                // Already all collapsed: nothing to restore to, so the last
                // press opens everything rather than snapping back.
                if !groups.iter().all(|g| collapsed.contains(g)) {
                    self.groups_before_cycle = Some(Before { collapsed: unlocked(&collapsed), home_only: unlocked(&home_only) });
                }
                self.collapsed_groups = all_collapsed;
                self.chipped_collapsed = keep(&folded);
                self.home_only_groups = keep(&home_only);
            }
            2 => {
                self.collapsed_groups = all_collapsed;
                self.chipped_collapsed = keep(&folded);
                self.home_only_groups = [keep(&home_only), self.cycled_homes(strip)].concat();
            }
            3 => {
                self.collapsed_groups = all_collapsed;
                self.chipped_collapsed = [keep(&folded), groups].concat();
                self.home_only_groups = keep(&home_only);
            }
            _ => {
                let before = self.groups_before_cycle.take().unwrap_or_default();
                self.collapsed_groups = [keep(&collapsed), unlocked(&before.collapsed)].concat();
                self.chipped_collapsed = keep(&folded);
                self.home_only_groups = [keep(&home_only), unlocked(&before.home_only)].concat();
            }
        }
    }

    /// The Pinned header's click (`togglePinnedSectionAtom`): open,
    /// collapsed, folded (skipped with nothing chipped), open.
    pub fn toggle_pinned(&mut self, has_chips: bool, visible: bool) {
        self.groups_before_cycle = None;
        if !visible {
            self.set_pinned_step(0);
        } else if !self.pinned_collapsed {
            self.set_pinned_step(1);
        } else if has_chips && !self.pinned_chipped_collapsed {
            self.set_pinned_step(3);
        } else {
            self.set_pinned_step(0);
        }
    }

    /// A group header's click (`toggleGroupCollapsedAtom`): open,
    /// collapsed, session only (with a home), folded (with chips), open.
    /// A locked group moves too: the lock only keeps ⌘0 out.
    pub fn toggle_group(&mut self, name: &str, has_chips: bool, has_home: bool, visible: bool) {
        self.groups_before_cycle = None;
        let step = self.group_step(name);
        let next = if !visible {
            Step::Open
        } else {
            match step {
                Step::Open => Step::Collapsed,
                Step::Collapsed if has_home => Step::Session,
                Step::Collapsed | Step::Session if has_chips => Step::Folded,
                _ => Step::Open,
            }
        };
        let with = |list: &[String], on: bool| {
            let mut l = without(list, name);
            if on {
                l.push(name.to_owned());
            }
            l
        };
        self.collapsed_groups = with(&self.collapsed_groups, next != Step::Open);
        self.home_only_groups = with(&self.home_only_groups, next == Step::Session);
        self.chipped_collapsed = with(&self.chipped_collapsed, next == Step::Folded);
    }

    pub fn toggle_group_lock(&mut self, name: &str) {
        let locked = self.is_locked(name);
        self.locked_groups = without(&self.locked_groups, name);
        if !locked {
            self.locked_groups.push(name.to_owned());
        }
    }

    /// Whether a row of `group` shows while the group is collapsed
    /// (`survives`): only a chipped one, and not once its chips fold or
    /// the group shows only its session.
    pub fn survives(&self, group: &str, chipped: bool) -> bool {
        let has = |list: &[String]| list.iter().any(|g| g == group);
        chipped && !has(&self.chipped_collapsed) && !has(&self.home_only_groups)
    }

    /// Whether group `name`'s rows show: all while it is open, the
    /// survivors while collapsed.
    pub fn row_shown(&self, group: &str, chipped: bool) -> bool {
        self.group_step(group) == Step::Open || self.survives(group, chipped)
    }

    /// Whether a pinned row shows (`shownPinned`).
    pub fn pinned_shown(&self, chipped: bool) -> bool {
        !self.pinned_collapsed || (!self.pinned_chipped_collapsed && chipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `pinnedCycle.check.ts`, step for step.
    fn strip(chipped: bool, homes: &[&str]) -> Strip {
        Strip { groups: vec!["a".into(), "b".into()], homes: homes.iter().map(|h| (*h).into()).collect(), chipped }
    }

    fn state(c: &Collapse) -> (bool, bool, Vec<String>, Vec<String>) {
        (c.pinned_collapsed, c.pinned_chipped_collapsed, c.collapsed_groups.clone(), c.chipped_collapsed.clone())
    }

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).into()).collect()
    }

    #[test]
    fn cmd_0_walks_collapsed_folded_open_and_restores_what_it_closed() {
        let s = strip(true, &[]);
        let mut c = Collapse { collapsed_groups: v(&["b"]), ..Default::default() };
        c.cycle(&s, true);
        assert_eq!(state(&c), (true, false, v(&["a", "b"]), v(&[])));
        c.cycle(&s, true);
        assert_eq!(state(&c), (true, true, v(&["a", "b"]), v(&["a", "b"])));
        c.cycle(&s, true);
        assert_eq!(state(&c), (false, false, v(&["b"]), v(&[])));
        c.cycle(&s, true);
        assert_eq!(state(&c), (true, false, v(&["a", "b"]), v(&[])));
        // Hidden, it only reveals: Pinned opens, the groups stay.
        c.cycle(&s, true);
        c.cycle(&s, false);
        assert_eq!(state(&c), (false, false, v(&["a", "b"]), v(&["a", "b"])));
    }

    #[test]
    fn one_press_lands_pinned_and_the_groups_on_the_same_step() {
        let s = strip(true, &[]);
        let mut c = Collapse { pinned_collapsed: true, ..Default::default() };
        c.cycle(&s, true);
        assert_eq!(state(&c), (true, false, v(&["a", "b"]), v(&[])));
        assert_eq!(c.cycle_step(&s), 1);
    }

    #[test]
    fn without_chips_the_cycle_is_two_presses_and_a_stranded_fold_still_opens() {
        let s = strip(false, &[]);
        let mut c = Collapse::default();
        c.cycle(&s, true);
        assert_eq!(state(&c), (true, false, v(&["a", "b"]), v(&[])));
        c.cycle(&s, true);
        assert_eq!(state(&c), (false, false, v(&[]), v(&[])));
        // Folded with chips, then the last chip closes: one press opens.
        let chipped = strip(true, &[]);
        c.cycle(&chipped, true);
        c.cycle(&chipped, true);
        assert_eq!(c.cycle_step(&chipped), 3);
        c.cycle(&s, true);
        assert_eq!(state(&c), (false, false, v(&[]), v(&[])));
    }

    #[test]
    fn a_header_click_skips_the_steps_that_would_show_nothing_new() {
        let mut c = Collapse::default();
        let mut click = |chips, home| {
            c.toggle_group("a", chips, home, true);
            c.group_step("a")
        };
        use Step::*;
        assert_eq!([0; 4].map(|_| click(true, true)), [Collapsed, Session, Folded, Open]);
        assert_eq!([0; 3].map(|_| click(true, false)), [Collapsed, Folded, Open]);
        assert_eq!([0; 3].map(|_| click(false, true)), [Collapsed, Session, Open]);
    }

    #[test]
    fn locks_keep_a_group_or_pinned_out_of_the_cycle_without_holding_it() {
        use Step::*;
        let s = strip(true, &[]);
        let mut c = Collapse { locked_groups: v(&["a"]), ..Default::default() };
        let seen: Vec<_> = (0..3)
            .map(|_| {
                c.cycle(&s, true);
                (c.group_step("a"), c.group_step("b"))
            })
            .collect();
        assert_eq!(seen, [(Open, Collapsed), (Open, Folded), (Open, Open)]);
        c.toggle_group("a", true, true, true);
        assert_eq!(c.group_step("a"), Collapsed);

        let mut c = Collapse { pinned_locked: true, ..Default::default() };
        let seen: Vec<_> = (0..3)
            .map(|_| {
                c.cycle(&s, true);
                (c.pinned_collapsed, c.group_step("a"))
            })
            .collect();
        assert_eq!(seen, [(false, Collapsed), (false, Folded), (false, Open)]);
    }

    #[test]
    fn the_last_press_puts_a_session_only_group_back() {
        use Step::*;
        let s = strip(true, &[]);
        let mut c = Collapse::default();
        c.toggle_group("b", true, true, true);
        c.toggle_group("b", true, true, true);
        assert_eq!(c.group_step("b"), Session);
        c.cycle(&s, true);
        c.cycle(&s, true);
        c.cycle(&s, true);
        assert_eq!((c.group_step("a"), c.group_step("b")), (Open, Session));
    }

    #[test]
    fn with_a_home_cmd_0_walks_four_steps() {
        use Step::*;
        let s = strip(true, &["a"]);
        let mut c = Collapse::default();
        let seen: Vec<_> = (0..4)
            .map(|_| {
                c.cycle(&s, true);
                (c.cycle_step(&s), c.group_step("a"), c.group_step("b"), c.pinned_collapsed, c.pinned_chipped_collapsed)
            })
            .collect();
        assert_eq!(
            seen,
            [
                (1, Collapsed, Collapsed, true, false),
                (2, Session, Collapsed, true, false),
                (3, Folded, Folded, true, true),
                (0, Open, Open, false, false),
            ]
        );
    }

    #[test]
    fn reads_bolts_saved_file() {
        let saved = r#"{"pinnedCollapsed":true,"pinnedLocked":true,"collapsedGroups":["x"],"homeOnlyGroups":["x"],"lockedGroups":["x"],"groupsBeforeCycle":null}"#;
        let c: Collapse = serde_json::from_str(saved).expect("parses");
        assert!(c.pinned_collapsed && c.pinned_locked);
        assert_eq!(c.group_step("x"), Step::Session);
        assert!(c.is_locked("x"));
    }
}
