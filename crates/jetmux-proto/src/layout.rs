//! Pane layouts: a binary split tree, and the geometry it produces.

use serde::{Deserialize, Serialize};

use crate::{Axis, Direction, LayoutSpec, PaneSpec, Rect};

pub type PaneId = u32;

fn axis_of(dir: Direction) -> Axis {
    match dir {
        Direction::Left | Direction::Right => Axis::Columns,
        Direction::Up | Direction::Down => Axis::Rows,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Layout {
    Pane(PaneId),
    Split {
        axis: Axis,
        /// Share of the space (minus the border) given to `first`.
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

const MIN_RATIO: f32 = 0.05;

impl Layout {
    /// Build a layout from a spec, taking an id for each pane in order.
    /// Returns `None` for a split with no children.
    pub fn from_spec(spec: &LayoutSpec, add: &mut impl FnMut(&PaneSpec) -> PaneId) -> Option<Self> {
        match spec {
            LayoutSpec::Pane(pane) => Some(Self::Pane(add(pane))),
            LayoutSpec::Split { axis, children } => Self::from_children(*axis, children, add),
        }
    }

    /// N weighted children become a chain of binary splits.
    fn from_children(
        axis: Axis,
        children: &[(u16, LayoutSpec)],
        add: &mut impl FnMut(&PaneSpec) -> PaneId,
    ) -> Option<Self> {
        let ((weight, first), rest) = children.split_first()?;
        let first = Self::from_spec(first, add)?;
        if rest.is_empty() {
            return Some(first);
        }
        let share = |w: u16| f32::from(w.max(1));
        let total: f32 = share(*weight) + rest.iter().map(|(w, _)| share(*w)).sum::<f32>();
        Some(Self::Split {
            axis,
            ratio: (share(*weight) / total).clamp(MIN_RATIO, 1.0 - MIN_RATIO),
            first: Box::new(first),
            second: Box::new(Self::from_children(axis, rest, add)?),
        })
    }

    /// Describe the layout as a spec, flattening chains of same-axis splits
    /// into one weighted list.
    pub fn to_spec(&self, pane: &impl Fn(PaneId) -> PaneSpec) -> LayoutSpec {
        match self {
            Self::Pane(id) => LayoutSpec::Pane(pane(*id)),
            Self::Split { axis, .. } => {
                let mut children = Vec::new();
                self.flatten(*axis, 1.0, pane, &mut children);
                LayoutSpec::Split {
                    axis: *axis,
                    children,
                }
            }
        }
    }

    fn flatten(
        &self,
        along: Axis,
        share: f32,
        pane: &impl Fn(PaneId) -> PaneSpec,
        out: &mut Vec<(u16, LayoutSpec)>,
    ) {
        match self {
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } if *axis == along => {
                first.flatten(along, share * ratio, pane, out);
                second.flatten(along, share * (1.0 - ratio), pane, out);
            }
            other => {
                // Weights are per mille of the parent split, at least 1.
                #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let weight = ((share * 1000.0).round() as u16).max(1);
                out.push((weight, other.to_spec(pane)));
            }
        }
    }

    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Pane(id) => out.push(*id),
            Self::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Self::Pane(p) => *p == id,
            Self::Split { first, second, .. } => first.contains(id) || second.contains(id),
        }
    }

    /// Split `target` and put `new` on the `dir` side of it.
    pub fn split(&mut self, target: PaneId, new: PaneId, dir: Direction) -> bool {
        match self {
            Self::Pane(id) if *id == target => {
                let (first, second) = match dir {
                    Direction::Right | Direction::Down => (target, new),
                    Direction::Left | Direction::Up => (new, target),
                };
                *self = Self::Split {
                    axis: axis_of(dir),
                    ratio: 0.5,
                    first: Box::new(Self::Pane(first)),
                    second: Box::new(Self::Pane(second)),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.split(target, new, dir) || second.split(target, new, dir)
            }
        }
    }

    /// Remove a pane, letting its sibling take its space. Returns `None` when
    /// the pane was the whole layout.
    pub fn remove(self, id: PaneId) -> Option<Self> {
        match self {
            Self::Pane(p) if p == id => None,
            Self::Pane(_) => Some(self),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => match (first.remove(id), second.remove(id)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    axis,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            },
        }
    }

    pub fn swap(&mut self, a: PaneId, b: PaneId) {
        match self {
            Self::Pane(id) if *id == a => *id = b,
            Self::Pane(id) if *id == b => *id = a,
            Self::Pane(_) => {}
            Self::Split { first, second, .. } => {
                first.swap(a, b);
                second.swap(a, b);
            }
        }
    }

    /// Lay out every pane inside `area`, with one-cell borders between them.
    pub fn rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.place(area, &mut out);
        out
    }

    fn place(&self, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Self::Pane(id) => out.push((*id, area)),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = divide(area, *axis, *ratio);
                first.place(a, out);
                second.place(b, out);
            }
        }
    }

    /// Move the border on the `dir` side of `id` by `cells` in that
    /// direction. If the pane has no border on that side, move the one on the
    /// opposite side instead, as tmux does.
    pub fn resize(&mut self, id: PaneId, dir: Direction, cells: u16, area: Rect) -> bool {
        let axis = axis_of(dir);
        // Path from the root to the pane: for each split on the way, whether
        // the pane is in `first`, and the split's area along `axis`.
        let mut path = Vec::new();
        if !self.path_to(id, area, &mut path) {
            return false;
        }
        let grows_forward = matches!(dir, Direction::Right | Direction::Down);
        // Moving the border after the pane (pane in `first`) is preferred for
        // Right/Down; the border before it (pane in `second`) for Left/Up.
        let pick = |want_first: bool| {
            path.iter()
                .rposition(|(a, in_first, _)| *a == axis && *in_first == want_first)
        };
        let Some(index) = pick(grows_forward).or_else(|| pick(!grows_forward)) else {
            return false;
        };
        let span = f32::from(path[index].2.saturating_sub(1).max(1));
        let delta = f32::from(cells) / span * if grows_forward { 1.0 } else { -1.0 };

        let mut node = self;
        for (_, in_first, _) in &path[..index] {
            let Self::Split { first, second, .. } = node else {
                return false;
            };
            node = if *in_first { first } else { second };
        }
        let Self::Split { ratio, .. } = node else {
            return false;
        };
        *ratio = (*ratio + delta).clamp(MIN_RATIO, 1.0 - MIN_RATIO);
        true
    }

    /// Set the `index`th split's ratio, counting splits in order (a split
    /// after everything in its first side, before its second).
    pub fn set_ratio(&mut self, index: usize, ratio: f32) -> bool {
        self.nth_split(&mut { index }).is_some_and(|r| {
            *r = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
            true
        })
    }

    fn nth_split(&mut self, left: &mut usize) -> Option<&mut f32> {
        let Self::Split {
            ratio,
            first,
            second,
            ..
        } = self
        else {
            return None;
        };
        if let Some(found) = first.nth_split(left) {
            return Some(found);
        }
        if *left == 0 {
            return Some(ratio);
        }
        *left -= 1;
        second.nth_split(left)
    }

    /// Every split's border within `area`, in the order [`Layout::set_ratio`]
    /// counts them: the border's cells, its axis, and the split's area.
    pub fn borders(&self, area: Rect) -> Vec<(Rect, Axis, Rect)> {
        let mut out = Vec::new();
        self.collect_borders(area, &mut out);
        out
    }

    fn collect_borders(&self, area: Rect, out: &mut Vec<(Rect, Axis, Rect)>) {
        if let Self::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        {
            let (a, b) = divide(area, *axis, *ratio);
            first.collect_borders(a, out);
            let border = match axis {
                Axis::Columns => Rect {
                    x: a.x + a.cols,
                    cols: 1,
                    ..area
                },
                Axis::Rows => Rect {
                    y: a.y + a.rows,
                    rows: 1,
                    ..area
                },
            };
            out.push((border, *axis, area));
            second.collect_borders(b, out);
        }
    }

    fn path_to(&self, id: PaneId, area: Rect, path: &mut Vec<(Axis, bool, u16)>) -> bool {
        match self {
            Self::Pane(p) => *p == id,
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let span = match axis {
                    Axis::Columns => area.cols,
                    Axis::Rows => area.rows,
                };
                let (a, b) = divide(area, *axis, *ratio);
                path.push((*axis, true, span));
                if first.path_to(id, a, path) {
                    return true;
                }
                path.pop();
                path.push((*axis, false, span));
                if second.path_to(id, b, path) {
                    return true;
                }
                path.pop();
                false
            }
        }
    }
}

fn divide(area: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    let span = match axis {
        Axis::Columns => area.cols,
        Axis::Rows => area.rows,
    };
    let usable = span.saturating_sub(1);
    // Truncation is fine: the value is clamped to `usable` right after.
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let first = ((f32::from(usable) * ratio).round() as u16).clamp(1.min(usable), usable);
    let second = usable - first;
    match axis {
        Axis::Columns => (
            Rect {
                cols: first,
                ..area
            },
            Rect {
                x: area.x + first + 1,
                cols: second,
                ..area
            },
        ),
        Axis::Rows => (
            Rect {
                rows: first,
                ..area
            },
            Rect {
                y: area.y + first + 1,
                rows: second,
                ..area
            },
        ),
    }
}

/// The pane next to `from` in direction `dir`: the closest one on that side
/// that overlaps it on the other axis.
pub fn neighbor(rects: &[(PaneId, Rect)], from: PaneId, dir: Direction) -> Option<PaneId> {
    let (_, f) = rects.iter().find(|(id, _)| *id == from)?;
    let (fx0, fy0) = (i32::from(f.x), i32::from(f.y));
    let (fx1, fy1) = (fx0 + i32::from(f.cols), fy0 + i32::from(f.rows));
    rects
        .iter()
        .filter(|(id, r)| *id != from && r.cols > 0 && r.rows > 0)
        .filter_map(|(id, r)| {
            let (x0, y0) = (i32::from(r.x), i32::from(r.y));
            let (x1, y1) = (x0 + i32::from(r.cols), y0 + i32::from(r.rows));
            let (gap, overlap, offset) = match dir {
                Direction::Right => (x0 - fx1, y1.min(fy1) - y0.max(fy0), (y0 - fy0).abs()),
                Direction::Left => (fx0 - x1, y1.min(fy1) - y0.max(fy0), (y0 - fy0).abs()),
                Direction::Down => (y0 - fy1, x1.min(fx1) - x0.max(fx0), (x0 - fx0).abs()),
                Direction::Up => (fy0 - y1, x1.min(fx1) - x0.max(fx0), (x0 - fx0).abs()),
            };
            (gap >= 0 && overlap > 0).then_some((gap, offset, *id))
        })
        .min()
        .map(|(_, _, id)| id)
}

#[cfg(test)]
mod tests {
    use crate::layout::{Layout, neighbor};
    use crate::{Axis, Direction, LayoutSpec, PaneSpec, Rect};

    const AREA: Rect = Rect {
        x: 0,
        y: 0,
        cols: 81,
        rows: 25,
    };

    /// 1 | 2
    ///   | -
    ///   | 3
    fn three() -> Layout {
        let mut l = Layout::Pane(1);
        assert!(l.split(1, 2, Direction::Right));
        assert!(l.split(2, 3, Direction::Down));
        l
    }

    #[test]
    fn borders_count_in_order_for_set_ratio() {
        let mut l = three();
        let borders: Vec<(u16, u16, Axis)> = l
            .borders(AREA)
            .into_iter()
            .map(|(b, axis, _)| (b.x, b.y, axis))
            .collect();
        // The vertical border between 1 and 2, then 2 and 3's.
        assert_eq!(borders, [(40, 0, Axis::Columns), (41, 12, Axis::Rows)]);
        assert!(l.set_ratio(1, 0.25));
        assert_eq!(l.rects(AREA)[1].1.rows, 6);
        assert!(!l.set_ratio(2, 0.5));
    }

    #[test]
    fn splits_lay_out_with_borders() {
        let rects = three().rects(AREA);
        assert_eq!(
            rects,
            vec![
                (
                    1,
                    Rect {
                        x: 0,
                        y: 0,
                        cols: 40,
                        rows: 25
                    }
                ),
                (
                    2,
                    Rect {
                        x: 41,
                        y: 0,
                        cols: 40,
                        rows: 12
                    }
                ),
                (
                    3,
                    Rect {
                        x: 41,
                        y: 13,
                        cols: 40,
                        rows: 12
                    }
                ),
            ]
        );
    }

    #[test]
    fn neighbors_follow_geometry() {
        let rects = three().rects(AREA);
        assert_eq!(neighbor(&rects, 1, Direction::Right), Some(2));
        assert_eq!(neighbor(&rects, 3, Direction::Left), Some(1));
        assert_eq!(neighbor(&rects, 2, Direction::Down), Some(3));
        assert_eq!(neighbor(&rects, 3, Direction::Up), Some(2));
        assert_eq!(neighbor(&rects, 1, Direction::Left), None);
    }

    #[test]
    fn remove_collapses_into_sibling() {
        let l = three().remove(2).unwrap();
        assert_eq!(l.panes(), vec![1, 3]);
        assert_eq!(
            l.rects(AREA)[1].1,
            Rect {
                x: 41,
                y: 0,
                cols: 40,
                rows: 25
            }
        );
        assert_eq!(Layout::Pane(1).remove(1), None);
    }

    #[test]
    fn resize_moves_the_nearest_border() {
        let mut l = three();
        assert!(l.resize(1, Direction::Right, 10, AREA));
        assert_eq!(l.rects(AREA)[0].1.cols, 50);
        // Pane 2 has no border to its right, so its left border moves.
        assert!(l.resize(2, Direction::Right, 10, AREA));
        assert_eq!(l.rects(AREA)[0].1.cols, 60);
        assert!(l.resize(3, Direction::Up, 2, AREA));
        assert_eq!(l.rects(AREA)[1].1.rows, 10);
        assert!(!Layout::Pane(1).resize(1, Direction::Up, 1, AREA));
    }

    #[test]
    fn specs_round_trip_through_layouts() {
        let pane = |cmd: &str| {
            LayoutSpec::Pane(PaneSpec {
                command: vec![cmd.into()],
                cwd: None,
            })
        };
        let spec = LayoutSpec::Split {
            axis: Axis::Columns,
            children: vec![
                (500, pane("a")),
                (
                    250,
                    LayoutSpec::Split {
                        axis: Axis::Rows,
                        children: vec![(1, pane("b")), (1, pane("c"))],
                    },
                ),
                (250, pane("d")),
            ],
        };
        let mut specs = Vec::new();
        let mut next = 0;
        let layout = Layout::from_spec(&spec, &mut |p: &PaneSpec| {
            specs.push(p.clone());
            next += 1;
            next - 1
        })
        .unwrap();
        assert_eq!(layout.panes(), vec![0, 1, 2, 3]);
        let rects = layout.rects(AREA);
        assert_eq!(rects[0].1.cols, 40);
        assert_eq!(rects[3].1.cols, 19);

        let saved = layout.to_spec(&|id| specs[id as usize].clone());
        let LayoutSpec::Split { axis, children } = &saved else {
            panic!("expected a split, got {saved:?}");
        };
        assert_eq!(*axis, Axis::Columns);
        assert_eq!(
            children.iter().map(|(w, _)| *w).collect::<Vec<_>>(),
            vec![500, 250, 250]
        );
        assert_eq!(
            Layout::from_spec(&saved, &mut |_| 0).unwrap().rects(AREA)[0]
                .1
                .cols,
            40
        );
    }
}
