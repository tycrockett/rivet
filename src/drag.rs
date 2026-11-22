//! Where a dragged row or group heading of the strip lands (Bolt's
//! `WorkspacesPanel` drag: `targetBefore` + `previewOrder`, its drop on a
//! group heading). The views register what can be dragged past
//! ([`slot`]); a drag measures them once as it starts ([`measure`]), and
//! [`Session`] then keeps [`drop_at`]'s answer for the pointer's place in
//! view as it moves: the rows it would pass slide out of the way
//! ([`offsets`], Bolt's `previewOrder` with its FLIP animation).
//!
//! Bolt reorders a row only inside its own group and moves it into a group
//! by a drop on the heading. Here a row also lands between another group's
//! rows (it joins that group) and below everything (it leaves its group).

use std::cell::RefCell;

use nsh_core::prelude::*;

/// Something drawn in the strip that a drag measures.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A pinned row, by path.
    Pinned(String),
    /// A tab's row, by path, in its group (`None` ungrouped).
    Row { path: String, group: Option<String> },
    /// A group's heading; `None` is the ungrouped "Tabs" heading.
    Header(Option<String>),
}

/// An item where it is drawn, in page coordinates.
#[derive(Debug, Clone)]
pub struct Slot {
    pub item: Item,
    pub top: f32,
    pub bottom: f32,
}

impl Slot {
    fn mid(&self) -> f32 {
        (self.top + self.bottom) / 2.0
    }
}

/// What a drop changes.
#[derive(Debug, Clone, PartialEq)]
pub enum Drop {
    /// The pinned row lands before `before`, else last.
    Pinned { before: Option<String> },
    /// The tab lands in `group` (`None` ungrouped) before `before`, else at
    /// the group's end.
    Tab { group: Option<String>, before: Option<String> },
    /// The group lands before group `before`, else last.
    Group { before: Option<String> },
}

/// Where `dragged`, drawn at `from` before the drag, lands with its middle
/// moved to `center`; `None` when it stays where it was.
pub fn drop_at(slots: &[Slot], dragged: &Item, from: &Slot, center: f32) -> Option<Drop> {
    match dragged {
        Item::Pinned(path) => {
            let before = slots
                .iter()
                .filter(|s| matches!(&s.item, Item::Pinned(p) if p != path))
                .find(|s| s.mid() > center)
                .map(|s| match &s.item {
                    Item::Pinned(p) => p.clone(),
                    _ => unreachable!(),
                });
            Some(Drop::Pinned { before })
        }
        Item::Header(Some(name)) => {
            let before = slots
                .iter()
                .filter_map(|s| match &s.item {
                    Item::Header(Some(g)) if g != name => Some((s, g)),
                    _ => None,
                })
                .find(|(s, _)| s.mid() > center)
                .map(|(_, g)| g.clone());
            Some(Drop::Group { before })
        }
        Item::Header(None) => None,
        Item::Row { path, .. } => {
            // Dropped on a heading: into that group, at its end.
            if let Some(Item::Header(group)) =
                slots.iter().find(|s| matches!(s.item, Item::Header(_)) && s.top <= center && center <= s.bottom).map(|s| &s.item)
            {
                return Some(Drop::Tab { group: group.clone(), before: None });
            }
            let items: Vec<&Slot> = slots
                .iter()
                .filter(|s| match &s.item {
                    Item::Row { path: p, .. } => p != path,
                    Item::Header(_) => true,
                    Item::Pinned(_) => false,
                })
                .collect();
            let Some(at) = items.iter().position(|s| s.mid() > center) else {
                // Below everything: out of its group, once it has cleared
                // where it was (a nudge of a group's last row keeps it).
                return (center > from.bottom).then_some(Drop::Tab { group: None, before: None });
            };
            match &items[at].item {
                Item::Row { path, group } => Some(Drop::Tab { group: group.clone(), before: Some(path.clone()) }),
                Item::Header(next) => match at.checked_sub(1).map(|i| &items[i].item) {
                    // Above a heading: the end of the section over it.
                    Some(Item::Row { group, .. }) | Some(Item::Header(group)) => Some(Drop::Tab { group: group.clone(), before: None }),
                    // Above the first heading (over Pinned): that group's top.
                    _ => {
                        let first = items[at + 1..].iter().find_map(|s| match &s.item {
                            Item::Row { path, group } if group == next => Some(path.clone()),
                            _ => None,
                        });
                        Some(Drop::Tab { group: next.clone(), before: first })
                    }
                },
                Item::Pinned(_) => unreachable!(),
            }
        }
    }
}

thread_local! {
    static SLOTS: RefCell<Vec<(u64, Item, ElementRef, AnimatedValue)>> = const { RefCell::new(Vec::new()) };
    static NEXT: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// Makes `el` something a drag measures and moves aside, for as long as the
/// view lives. `pan` is the value a drag of `el` itself moves natively; it
/// moves with the shift a drag of another item gives, in one transform.
pub fn slot<E: Element>(el: E, item: Item, pan: Option<&AnimatedValue>) -> E {
    let handle = element_ref();
    let shift = animated_value(0.0);
    let key = NEXT.with(|n| {
        let k = n.get();
        n.set(k + 1);
        k
    });
    SLOTS.with(|s| s.borrow_mut().push((key, item, handle, shift)));
    cleanup(move || SLOTS.with(|s| s.borrow_mut().retain(|(k, ..)| *k != key)));
    let el = el.element_ref(&handle);
    match pan {
        Some(pan) => el.bind_translate_y(&pan.add(&shift)),
        None => el.bind_translate_y(&shift),
    }
}

/// Every registered item where it is drawn, top to bottom. Layout only:
/// the shifts and pans in flight do not move what is measured.
pub async fn measure() -> Vec<Slot> {
    let items: Vec<(Item, ElementRef)> = SLOTS.with(|s| s.borrow().iter().map(|(_, i, e, _)| (i.clone(), *e)).collect());
    let mut out = Vec::new();
    for (item, el) in items {
        if let Some(m) = el.measure().await {
            let top = m.page.y.0;
            out.push(Slot { item, top, bottom: top + m.frame.size.height.0 });
        }
    }
    out.sort_by(|a, b| a.top.total_cmp(&b.top));
    out
}

/// The shift of `item`, if it is drawn.
fn shift_of(item: &Item) -> Option<AnimatedValue> {
    SLOTS.with(|s| s.borrow().iter().find(|(_, i, ..)| i == item).map(|(.., v)| *v))
}

/// How rows make way, and how the dragged card settles: no overshoot.
fn spring() -> Spring {
    Spring::new(200.0).damping_ratio(1.0)
}

/// Springs the dragged card's pan value to `to` (what [`Session::release`]
/// says is left to go); done when it has settled.
pub async fn settle(pan: AnimatedValue, to: f32) {
    pan.spring(to, spring()).await;
}

/// What moves with `dragged` (indices of `slots`, itself first): a group's
/// heading takes the group's rows along, as [`Drop::Group`] moves them.
pub fn block(slots: &[Slot], dragged: &Item) -> Vec<usize> {
    let Some(at) = slots.iter().position(|s| s.item == *dragged) else { return Vec::new() };
    let mut out = vec![at];
    if let Item::Header(Some(name)) = dragged {
        out.extend(
            (at + 1..slots.len()).take_while(|&i| matches!(&slots[i].item, Item::Row { group: Some(g), .. } if g == name)),
        );
    }
    out
}

/// The order of `slots` (as indices) once `dragged` has landed as `drop` says.
fn reordered(slots: &[Slot], dragged: &Item, drop: &Drop) -> Vec<usize> {
    let moving = block(slots, dragged);
    let Some(&first) = moving.first() else { return (0..slots.len()).collect() };
    let rest: Vec<usize> = (0..slots.len()).filter(|i| !moving.contains(i)).collect();
    let find = |want: &Item| rest.iter().position(|&i| slots[i].item == *want);
    // After the last of `rest` that `of` picks.
    let after_last = |of: &dyn Fn(&Item) -> bool| rest.iter().rposition(|&i| of(&slots[i].item)).map(|p| p + 1);
    // Where it was: what lands nowhere new stays.
    let home = rest.iter().take_while(|&&i| i < first).count();
    let at = match drop {
        Drop::Pinned { before } => before
            .as_ref()
            .and_then(|b| find(&Item::Pinned(b.clone())))
            .or_else(|| after_last(&|i| matches!(i, Item::Pinned(_)))),
        Drop::Tab { group, before: Some(b) } => find(&Item::Row { path: b.clone(), group: group.clone() }),
        Drop::Tab { group: Some(g), before: None } => {
            after_last(&|i| matches!(i, Item::Row { group: Some(r), .. } | Item::Header(Some(r)) if r == g))
        }
        // The ungrouped rows' end; with none, below everything.
        Drop::Tab { group: None, before: None } => {
            after_last(&|i| matches!(i, Item::Row { group: None, .. } | Item::Header(None))).or(Some(rest.len()))
        }
        Drop::Group { before: Some(b) } => find(&Item::Header(Some(b.clone()))),
        // The last group's end: above the ungrouped heading.
        Drop::Group { before: None } => find(&Item::Header(None)).or(Some(rest.len())),
    }
    .unwrap_or(home);
    let mut order = rest;
    order.splice(at..at, moving);
    order
}

/// How far each of `slots` (parallel) sits from where it is drawn once
/// `dragged` has landed as `drop` says. The rows are stacked in their new
/// order, each taking the room from its top to the next one's (so the
/// margins a heading carries go with it).
pub fn offsets(slots: &[Slot], dragged: &Item, drop: &Drop) -> Vec<f32> {
    let mut out = vec![0.0; slots.len()];
    let Some(origin) = slots.first().map(|s| s.top) else { return out };
    let room = |i: usize| match slots.get(i + 1) {
        Some(next) => next.top - slots[i].top,
        None => slots[i].bottom - slots[i].top,
    };
    let mut top = origin;
    for i in reordered(slots, dragged, drop) {
        out[i] = top - slots[i].top;
        top += room(i);
    }
    out
}

/// One drag of `dragged` once its slots are measured: it follows the
/// pointer natively; this slides the rows around it out of the way as the
/// drop it would make changes, and at its end settles them where they land.
pub struct Session {
    slots: Vec<Slot>,
    /// The dragged item's index in `slots`.
    dragged: usize,
    /// What moves with it ([`block`]).
    block: Vec<usize>,
    from: Slot,
    /// Where each slot was last sent, so a move that changes nothing sends
    /// nothing.
    sent: Vec<f32>,
}

impl Session {
    /// A drag of `dragged` over `slots` as drawn before it; `None` if it is
    /// not among them.
    pub fn start(slots: Vec<Slot>, dragged: &Item) -> Option<Session> {
        let at = slots.iter().position(|s| s.item == *dragged)?;
        let block = block(&slots, dragged);
        let from = slots[at].clone();
        let sent = vec![0.0; slots.len()];
        Some(Session { slots, dragged: at, block, from, sent })
    }

    /// The drop with the card's middle `by` from where it was, and where
    /// every slot would then sit.
    fn plan(&self, by: f32) -> (Option<Drop>, Vec<f32>) {
        let item = &self.slots[self.dragged].item;
        let drop = drop_at(&self.slots, item, &self.from, self.from.mid() + by);
        let offsets = match &drop {
            Some(drop) => offsets(&self.slots, item, drop),
            None => vec![0.0; self.slots.len()],
        };
        (drop, offsets)
    }

    /// The pointer is `by` from where the drag began: the rows in the way
    /// slide aside, a group's rows follow its heading.
    pub fn moved(&mut self, by: f32) {
        let (_, offsets) = self.plan(by);
        for (i, &to) in offsets.iter().enumerate() {
            if i == self.dragged {
                continue;
            }
            let Some(shift) = shift_of(&self.slots[i].item) else { continue };
            if self.block.contains(&i) {
                shift.set(by);
            } else if (self.sent[i] - to).abs() > 0.5 {
                self.sent[i] = to;
                let _ = shift.spring(to, spring());
            }
        }
    }

    /// Let go `by` from where the drag began: everything else springs to
    /// where the drop puts it. Returns the drop, and how far the dragged
    /// card itself has left to go (its own pan value is the caller's to
    /// [`settle`]).
    pub fn release(&mut self, by: f32) -> (Option<Drop>, f32) {
        let (drop, offsets) = self.plan(by);
        for (i, &to) in offsets.iter().enumerate() {
            if i == self.dragged || (!self.block.contains(&i) && (self.sent[i] - to).abs() <= 0.5) {
                continue;
            }
            self.sent[i] = to;
            if let Some(shift) = shift_of(&self.slots[i].item) {
                let _ = shift.spring(to, spring());
            }
        }
        (drop, offsets[self.dragged])
    }

    /// Everything back where it was drawn: the drop changed nothing after
    /// all (the card's own pan value is the caller's to [`settle`]).
    pub fn cancel(&mut self) {
        for (i, sent) in self.sent.iter_mut().enumerate() {
            if i != self.dragged {
                *sent = 0.0;
                if let Some(shift) = shift_of(&self.slots[i].item) {
                    let _ = shift.spring(0.0, spring());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, group: Option<&str>, top: f32) -> Slot {
        Slot { item: Item::Row { path: path.into(), group: group.map(Into::into) }, top, bottom: top + 30.0 }
    }

    fn head(group: Option<&str>, top: f32) -> Slot {
        Slot { item: Item::Header(group.map(Into::into)), top, bottom: top + 20.0 }
    }

    // a: [a1 a2]  b: [b1]  Tabs: [t1]
    fn strip() -> Vec<Slot> {
        vec![
            head(Some("a"), 0.0),
            row("a1", Some("a"), 20.0),
            row("a2", Some("a"), 50.0),
            head(Some("b"), 80.0),
            row("b1", Some("b"), 100.0),
            head(None, 130.0),
            row("t1", None, 150.0),
        ]
    }

    fn drag(path: &str, by: f32) -> Option<Drop> {
        let slots = strip();
        let from = slots.iter().find(|s| matches!(&s.item, Item::Row { path: p, .. } if p == path)).unwrap().clone();
        drop_at(&slots, &from.item, &from, from.mid() + by)
    }

    fn tab(group: Option<&str>, before: Option<&str>) -> Option<Drop> {
        Some(Drop::Tab { group: group.map(Into::into), before: before.map(Into::into) })
    }

    #[test]
    fn a_row_reorders_inside_its_group() {
        assert_eq!(drag("a2", -35.0), tab(Some("a"), Some("a1")));
        assert_eq!(drag("a1", 35.0), tab(Some("a"), None));
    }

    #[test]
    fn a_nudged_last_row_stays_in_its_group() {
        assert_eq!(drag("a2", 4.0), tab(Some("a"), None));
        assert_eq!(drag("t1", 10.0), None);
    }

    #[test]
    fn a_row_joins_the_group_it_lands_in_or_the_heading_it_lands_on() {
        assert_eq!(drag("a1", 70.0), tab(Some("b"), Some("b1")));
        assert_eq!(drag("a1", 55.0), tab(Some("b"), None));
        assert_eq!(drag("b1", 120.0), tab(None, None));
    }

    #[test]
    fn a_row_dropped_below_everything_leaves_its_group() {
        let slots = vec![head(Some("a"), 0.0), row("a1", Some("a"), 20.0), row("a2", Some("a"), 50.0)];
        let from = slots[2].clone();
        assert_eq!(drop_at(&slots, &from.item, &from, from.mid() + 40.0), tab(None, None));
    }

    #[test]
    fn a_heading_reorders_the_groups() {
        let slots = strip();
        let from = slots[3].clone();
        assert_eq!(drop_at(&slots, &from.item, &from, -5.0), Some(Drop::Group { before: Some("a".into()) }));
        let from = slots[0].clone();
        assert_eq!(drop_at(&slots, &from.item, &from, 120.0), Some(Drop::Group { before: None }));
    }

    fn offs(dragged: &str, drop: Drop) -> Vec<f32> {
        let slots = strip();
        let item = slots.iter().map(|s| s.item.clone()).find(|i| match i {
            Item::Row { path, .. } => path == dragged,
            Item::Header(Some(g)) => g == dragged,
            _ => false,
        });
        offsets(&slots, &item.unwrap(), &drop)
    }

    fn tab_drop(group: Option<&str>, before: Option<&str>) -> Drop {
        Drop::Tab { group: group.map(Into::into), before: before.map(Into::into) }
    }

    #[test]
    fn a_heading_takes_its_rows_along() {
        let slots = strip();
        assert_eq!(block(&slots, &slots[0].item), vec![0, 1, 2]);
        assert_eq!(block(&slots, &slots[3].item), vec![3, 4]);
        assert_eq!(block(&slots, &slots[2].item), vec![2]);
        assert_eq!(block(&slots, &slots[5].item), vec![5]);
    }

    #[test]
    fn a_row_passed_slides_by_the_height_of_the_card() {
        // a1 below a2: a2 moves up into a1's room, a1 down by a2's.
        assert_eq!(offs("a1", tab_drop(Some("a"), None)), vec![0.0, 30.0, -30.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(offs("a2", tab_drop(Some("a"), Some("a1"))), vec![0.0, 30.0, -30.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn nothing_slides_when_the_drop_changes_nothing() {
        assert_eq!(offs("a2", tab_drop(Some("a"), None)), vec![0.0; 7]);
        assert_eq!(offs("a1", tab_drop(Some("a"), Some("a2"))), vec![0.0; 7]);
        assert_eq!(offs("a", Drop::Group { before: Some("b".into()) }), vec![0.0; 7]);
    }

    #[test]
    fn a_row_joining_a_group_opens_a_gap_in_it() {
        // a1 before b1: a2 and b's heading move up by a1's 30, a1 lands
        // under the heading, b1 stays where it is.
        assert_eq!(offs("a1", tab_drop(Some("b"), Some("b1"))), vec![0.0, 50.0, -30.0, -30.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn a_row_leaving_for_the_ungrouped_ones_lands_below_them() {
        // b1 at the end of Tabs: the Tabs heading and t1 move up by its 30.
        assert_eq!(offs("b1", tab_drop(None, None)), vec![0.0, 0.0, 0.0, 0.0, 50.0, -30.0, -30.0]);
    }

    #[test]
    fn a_group_moves_with_its_rows_past_another() {
        // b (heading and b1: 50 high) above a: a's three rows move down 50,
        // b's two up 80.
        assert_eq!(offs("b", Drop::Group { before: Some("a".into()) }), vec![50.0, 50.0, 50.0, -80.0, -80.0, 0.0, 0.0]);
        // a last: b's rows move up by a's 80, a's down by b's 50.
        assert_eq!(offs("a", Drop::Group { before: None }), vec![50.0, 50.0, 50.0, -80.0, -80.0, 0.0, 0.0]);
    }

    #[test]
    fn a_pinned_row_passes_pinned_rows_only() {
        let slots = vec![
            Slot { item: Item::Pinned("p1".into()), top: 0.0, bottom: 30.0 },
            Slot { item: Item::Pinned("p2".into()), top: 30.0, bottom: 60.0 },
            head(Some("a"), 60.0),
            row("a1", Some("a"), 80.0),
        ];
        let p1 = Item::Pinned("p1".into());
        assert_eq!(offsets(&slots, &p1, &Drop::Pinned { before: None }), vec![30.0, -30.0, 0.0, 0.0]);
        assert_eq!(offsets(&slots, &p1, &Drop::Pinned { before: Some("p2".into()) }), vec![0.0; 4]);
    }
}
