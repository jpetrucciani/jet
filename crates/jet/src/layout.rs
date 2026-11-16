//! A tab's split tree: which panes it holds and where each one goes.
//! Plain data and integer pixels, so it tests on any platform.

use jet_config::keybinds::Direction;

pub type PaneId = u64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        (self.x..self.x + self.w).contains(&x) && (self.y..self.y + self.h).contains(&y)
    }
}

/// The gap between a split's two sides, which drags to move it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Divider {
    pub rect: Rect,
    /// Between side-by-side panes (a vertical line).
    pub horizontal: bool,
    /// The whole split's area.
    pub area: Rect,
}

impl Divider {
    /// The split ratio that puts this divider's middle at client `(x, y)`,
    /// leaving at least `min` pixels on either side when there's room.
    pub fn ratio_at(&self, x: i32, y: i32, min: i32) -> f32 {
        let (start, total, at, gap) = if self.horizontal {
            (self.area.x, self.area.w, x, self.rect.w)
        } else {
            (self.area.y, self.area.h, y, self.rect.h)
        };
        let span = (total - gap).max(1);
        let low = min.min(span / 2);
        let first = (at - start - gap / 2).clamp(low, span - low);
        first as f32 / span as f32
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Leaf(PaneId),
    Split {
        /// Side by side (`true`) or stacked.
        horizontal: bool,
        /// Share of the space the first child gets.
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

impl Node {
    /// Put `new` beside `target`, on its `side`. False if `target` isn't here.
    pub fn split(&mut self, target: PaneId, new: PaneId, side: Direction) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                let (old, new) = (Box::new(Self::Leaf(target)), Box::new(Self::Leaf(new)));
                let before = matches!(side, Direction::Left | Direction::Up);
                let (first, second) = if before { (new, old) } else { (old, new) };
                *self = Self::Split {
                    horizontal: matches!(side, Direction::Left | Direction::Right),
                    ratio: 0.5,
                    first,
                    second,
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, new, side) || second.split(target, new, side)
            }
        }
    }

    /// The tree without `target`; its sibling takes the space. `None` if it
    /// was the only pane.
    pub fn without(self, target: PaneId) -> Option<Self> {
        match self {
            Self::Leaf(id) if id == target => None,
            leaf @ Self::Leaf(_) => Some(leaf),
            Self::Split {
                horizontal,
                ratio,
                first,
                second,
            } => match (first.without(target), second.without(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    horizontal,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (one, other) => one.or(other),
            },
        }
    }

    /// Give `pane` `share` (0 to 1) of the split it sits in. False if it
    /// isn't in a split.
    pub fn share(&mut self, pane: PaneId, share: f32) -> bool {
        let Self::Split {
            ratio,
            first,
            second,
            ..
        } = self
        else {
            return false;
        };
        let share = share.clamp(0.05, 0.95);
        if **first == Self::Leaf(pane) {
            *ratio = share;
            true
        } else if **second == Self::Leaf(pane) {
            *ratio = 1.0 - share;
            true
        } else {
            first.share(pane, share) || second.share(pane, share)
        }
    }

    /// Set the ratio of the split whose divider [`Node::layout`] listed at
    /// `index`. False if there's no such split.
    pub fn set_ratio(&mut self, index: usize, ratio: f32) -> bool {
        self.find_split(&mut { index }, ratio)
    }

    /// In-order, as `layout` lists dividers; `left` counts down to the one.
    fn find_split(&mut self, left: &mut usize, new: f32) -> bool {
        let Self::Split {
            ratio,
            first,
            second,
            ..
        } = self
        else {
            return false;
        };
        if first.find_split(left, new) {
            return true;
        }
        if *left == 0 {
            *ratio = new;
            return true;
        }
        *left -= 1;
        second.find_split(left, new)
    }

    /// Panes in reading order (left to right, top to bottom).
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    /// Each pane's rect within `area`, with `gap` pixels between splits
    /// (returned as dividers, in order).
    pub fn layout(
        &self,
        area: Rect,
        gap: i32,
        panes: &mut Vec<(PaneId, Rect)>,
        dividers: &mut Vec<Divider>,
    ) {
        match self {
            Self::Leaf(id) => panes.push((*id, area)),
            Self::Split {
                horizontal,
                ratio,
                first,
                second,
            } => {
                let total = if *horizontal { area.w } else { area.h };
                let a =
                    (((total - gap) as f32 * ratio).round() as i32).clamp(0, (total - gap).max(0));
                let b = (total - gap - a).max(0);
                let (ra, divider, rb) = if *horizontal {
                    (
                        Rect { w: a, ..area },
                        Rect {
                            x: area.x + a,
                            w: gap,
                            ..area
                        },
                        Rect {
                            x: area.x + a + gap,
                            w: b,
                            ..area
                        },
                    )
                } else {
                    (
                        Rect { h: a, ..area },
                        Rect {
                            y: area.y + a,
                            h: gap,
                            ..area
                        },
                        Rect {
                            y: area.y + a + gap,
                            h: b,
                            ..area
                        },
                    )
                };
                first.layout(ra, gap, panes, dividers);
                dividers.push(Divider {
                    rect: divider,
                    horizontal: *horizontal,
                    area,
                });
                second.layout(rb, gap, panes, dividers);
            }
        }
    }
}

/// The pane next to `from` toward `dir`: of those whose edge faces it and
/// that overlap it sideways, the nearest, then the most overlapping.
pub fn neighbor(rects: &[(PaneId, Rect)], from: PaneId, dir: Direction) -> Option<PaneId> {
    let (_, f) = rects.iter().find(|(id, _)| *id == from)?;
    let overlap = |a0: i32, a1: i32, b0: i32, b1: i32| a1.min(b1) - a0.max(b0);
    rects
        .iter()
        .filter(|(id, _)| *id != from)
        .filter_map(|(id, r)| {
            let (gap, side) = match dir {
                Direction::Left => (f.x - (r.x + r.w), overlap(f.y, f.y + f.h, r.y, r.y + r.h)),
                Direction::Right => (r.x - (f.x + f.w), overlap(f.y, f.y + f.h, r.y, r.y + r.h)),
                Direction::Up => (f.y - (r.y + r.h), overlap(f.x, f.x + f.w, r.x, r.x + r.w)),
                Direction::Down => (r.y - (f.y + f.h), overlap(f.x, f.x + f.w, r.x, r.x + r.w)),
            };
            (gap >= 0 && side > 0).then_some((gap, -side, *id))
        })
        .min()
        .map(|(_, _, id)| id)
}

#[cfg(test)]
mod tests {
    use jet_config::keybinds::Direction;

    use crate::layout::{Divider, Node, Rect, neighbor};

    fn layout(node: &Node) -> (Vec<(u64, Rect)>, Vec<Divider>) {
        let (mut panes, mut dividers) = (Vec::new(), Vec::new());
        let area = Rect {
            x: 0,
            y: 0,
            w: 101,
            h: 50,
        };
        node.layout(area, 1, &mut panes, &mut dividers);
        (panes, dividers)
    }

    #[test]
    fn splits_share_space_and_closing_gives_it_back() {
        let mut tree = Node::Leaf(1);
        assert!(tree.split(1, 2, Direction::Right));
        assert!(tree.split(2, 3, Direction::Down));
        assert!(!tree.split(9, 4, Direction::Down));
        let (panes, dividers) = layout(&tree);
        assert_eq!(
            panes,
            [
                (
                    1,
                    Rect {
                        x: 0,
                        y: 0,
                        w: 50,
                        h: 50
                    }
                ),
                (
                    2,
                    Rect {
                        x: 51,
                        y: 0,
                        w: 50,
                        h: 25
                    }
                ),
                (
                    3,
                    Rect {
                        x: 51,
                        y: 26,
                        w: 50,
                        h: 24
                    }
                ),
            ]
        );
        assert_eq!(dividers.len(), 2);
        assert!(panes[2].1.contains(51, 26) && !panes[2].1.contains(51, 25));

        let tree = tree.without(2).unwrap_or(Node::Leaf(0));
        assert_eq!(tree.panes(), [1, 3]);
        let (panes, _) = layout(&tree);
        assert_eq!(
            panes[1],
            (
                3,
                Rect {
                    x: 51,
                    y: 0,
                    w: 50,
                    h: 50
                }
            )
        );
        assert_eq!(Node::Leaf(1).without(1), None);
    }

    #[test]
    fn left_and_up_put_the_new_pane_first() {
        let mut tree = Node::Leaf(1);
        tree.split(1, 2, Direction::Up);
        assert_eq!(tree.panes(), [2, 1]);
    }

    #[test]
    fn neighbors_follow_geometry() {
        // 1 | 2
        //   | 3
        let mut tree = Node::Leaf(1);
        tree.split(1, 2, Direction::Right);
        tree.split(2, 3, Direction::Down);
        let (rects, _) = layout(&tree);
        assert_eq!(neighbor(&rects, 1, Direction::Right), Some(2));
        assert_eq!(neighbor(&rects, 3, Direction::Left), Some(1));
        assert_eq!(neighbor(&rects, 3, Direction::Up), Some(2));
        assert_eq!(neighbor(&rects, 2, Direction::Down), Some(3));
        assert_eq!(neighbor(&rects, 1, Direction::Left), None);
    }

    #[test]
    fn dragging_a_divider_moves_its_split() {
        // 1 | 2
        //   | 3
        let mut tree = Node::Leaf(1);
        tree.split(1, 2, Direction::Right);
        tree.split(2, 3, Direction::Down);
        let (_, dividers) = layout(&tree);
        let (outer, inner) = (dividers[0], dividers[1]);
        assert!(outer.horizontal && !inner.horizontal);

        // The outer divider to x = 30, then the inner one as far up as it goes.
        assert!(tree.set_ratio(0, outer.ratio_at(30, 0, 10)));
        assert!(tree.set_ratio(1, inner.ratio_at(0, -100, 10)));
        assert!(!tree.set_ratio(2, 0.5));
        let (panes, dividers) = layout(&tree);
        assert_eq!(dividers[0].rect.x, 30);
        assert_eq!(panes[0].1.w, 30);
        assert_eq!(panes[1].1.h, 10);
    }

    #[test]
    fn a_pane_takes_its_share_from_either_side() {
        let mut root = Node::Leaf(1);
        root.split(1, 2, Direction::Right);
        root.split(2, 3, Direction::Down);
        assert!(root.share(3, 0.25));
        assert!(root.share(1, 0.7));
        let Node::Split { ratio, second, .. } = &root else {
            return;
        };
        assert!((ratio - 0.7).abs() < 1e-6);
        assert!(matches!(**second, Node::Split { ratio, .. } if (ratio - 0.75).abs() < 1e-6));
        assert!(!Node::Leaf(1).share(1, 0.5));
    }
}
