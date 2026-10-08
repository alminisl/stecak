//! Split layout for a tab: a binary tree whose leaves are panes.

use crate::pane::PaneId;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Dir {
    /// Children side by side (vertical divider).
    Horizontal,
    /// Children stacked (horizontal divider).
    Vertical,
}

#[derive(Debug)]
pub enum Node {
    Leaf(PaneId),
    Split { dir: Dir, ratio: f32, a: Box<Node>, b: Box<Node> },
}

/// Width of the gap between split panes, in physical pixels per unit of scale.
pub const DIVIDER: f32 = 1.0;

impl Node {
    /// Pane rectangles plus divider rectangles for this subtree within `r`.
    pub fn layout(&self, r: Rect, gap: f32, panes: &mut Vec<(PaneId, Rect)>, dividers: &mut Vec<Rect>) {
        match self {
            Node::Leaf(id) => panes.push((*id, r)),
            Node::Split { dir, ratio, a, b } => {
                let (ra, rb, div) = split_rect(r, *dir, *ratio, gap);
                dividers.push(div);
                a.layout(ra, gap, panes, dividers);
                b.layout(rb, gap, panes, dividers);
            }
        }
    }

    /// Replace leaf `target` with a split of (target, new).
    pub fn split(&mut self, target: PaneId, new: PaneId, dir: Dir) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split { dir, ratio: 0.5, a: Box::new(Node::Leaf(target)), b: Box::new(Node::Leaf(new)) };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(target, new, dir) || b.split(target, new, dir),
        }
    }

    /// Remove leaf `target`, collapsing its parent split. Returns false if this node *is*
    /// that leaf (the caller must drop the whole node).
    pub fn remove(&mut self, target: PaneId) -> bool {
        match self {
            Node::Leaf(id) => *id != target,
            Node::Split { a, b, .. } => {
                if matches!(**a, Node::Leaf(id) if id == target) {
                    *self = std::mem::replace(&mut **b, Node::Leaf(0));
                } else if matches!(**b, Node::Leaf(id) if id == target) {
                    *self = std::mem::replace(&mut **a, Node::Leaf(0));
                } else {
                    a.remove(target);
                    b.remove(target);
                }
                true
            }
        }
    }

    pub fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    pub fn first_leaf(&self) -> PaneId {
        match self {
            Node::Leaf(id) => *id,
            Node::Split { a, .. } => a.first_leaf(),
        }
    }
}

fn split_rect(r: Rect, dir: Dir, ratio: f32, gap: f32) -> (Rect, Rect, Rect) {
    match dir {
        Dir::Horizontal => {
            let wa = ((r.w - gap) * ratio).round();
            let a = Rect { w: wa, ..r };
            let div = Rect { x: r.x + wa, w: gap, ..r };
            let b = Rect { x: r.x + wa + gap, w: r.w - wa - gap, ..r };
            (a, b, div)
        }
        Dir::Vertical => {
            let ha = ((r.h - gap) * ratio).round();
            let a = Rect { h: ha, ..r };
            let div = Rect { y: r.y + ha, h: gap, ..r };
            let b = Rect { y: r.y + ha + gap, h: r.h - ha - gap, ..r };
            (a, b, div)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_remove() {
        let mut root = Node::Leaf(1);
        assert!(root.split(1, 2, Dir::Horizontal));
        assert!(root.split(2, 3, Dir::Vertical));
        let mut ids = vec![];
        root.leaves(&mut ids);
        assert_eq!(ids, vec![1, 2, 3]);

        let (mut panes, mut divs) = (vec![], vec![]);
        root.layout(Rect { x: 0.0, y: 0.0, w: 201.0, h: 101.0 }, 1.0, &mut panes, &mut divs);
        assert_eq!(panes.len(), 3);
        assert_eq!(divs.len(), 2);
        assert_eq!(panes[0].1.w, 100.0);

        assert!(root.remove(2));
        ids.clear();
        root.leaves(&mut ids);
        assert_eq!(ids, vec![1, 3]);
        assert!(!Node::Leaf(7).remove(7));
    }
}
