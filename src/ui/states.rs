// Runtime states of a UI template, shown one at a time. Templates author
// mutually exclusive groups on top of each other (the quest tracker's
// "no quests" / tracked / untracked lists all at one spot) and let game
// code pick one, and they carry helpers the game positions or never draws
// (blockers, scroll bars, focus frames). The canvas shows one state per
// spot and hides the helpers unless asked.
use super::canvas::{Kind, Node};
use std::collections::HashMap;

/// Siblings whose boxes start within this many design pixels of each
/// other are alternatives for the same spot.
const SAME_SPOT: f32 = 2.;

/// Name prefixes of widgets the game places or toggles itself. Matching by
/// name is a heuristic; these names are consistent across the shipped
/// templates (`Blocker` input catchers, `ScrollBar_DS`, `Focus_DS` gamepad
/// frames, `InfoTipWidget` hover anchors, `fake_mask` placeholders,
/// `BadgeIconState_*` notification badges).
const HELPERS: [&str; 7] = [
    "Blocker",
    "ScrollBar",
    "Focus",
    "InfoTipWidget",
    "fake_mask",
    "Gamepad",
    // Warning and new-item badges on buttons: off in an idle game.
    "BadgeIconState",
];

/// A spot with several alternative groups; `members` are node indices.
#[derive(Clone, Debug, PartialEq)]
pub struct StateGroup {
    pub parent: usize,
    pub members: Vec<usize>,
}

/// The parent of every node, from the pre-order depth list.
pub fn parents(nodes: &[Node]) -> Vec<Option<usize>> {
    let mut stack: Vec<usize> = Vec::new();
    nodes
        .iter()
        .enumerate()
        .map(|(i, n)| {
            while stack.last().is_some_and(|&p| nodes[p].depth >= n.depth) {
                stack.pop();
            }
            let parent = stack.last().copied();
            stack.push(i);
            parent
        })
        .collect()
}

/// Groups that share a spot under one parent (the design view's guess;
/// with bindings on, the bindings pick states). Only visible containers with
/// content count: a background image under a group is not an alternative.
pub fn groups(nodes: &[Node]) -> Vec<StateGroup> {
    let parents = parents(nodes);
    let has_children = |i: usize| nodes.get(i + 1).is_some_and(|n| n.depth > nodes[i].depth);
    let mut by_parent: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if let Some(p) = parents[i]
            && n.visible
            && n.kind == Kind::Container
            && has_children(i)
        {
            by_parent.entry(p).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    for (parent, children) in by_parent {
        let mut left = children;
        while let Some(first) = left.first().copied() {
            let at = |i: usize| [nodes[i].rect[0], nodes[i].rect[1]];
            let (same, rest): (Vec<usize>, Vec<usize>) = left.into_iter().partition(|&i| {
                let (a, b) = (at(first), at(i));
                (a[0] - b[0]).abs() <= SAME_SPOT && (a[1] - b[1]).abs() <= SAME_SPOT
            });
            if same.len() > 1 {
                out.push(StateGroup {
                    parent,
                    members: same,
                });
            }
            left = rest;
        }
    }
    out.sort_by_key(|g| g.members[0]);
    out
}

pub fn is_helper(node: &Node) -> bool {
    // Case-blind: designers write both `Gamepad…` and `GamePad…`.
    node.runtime
        || HELPERS.iter().any(|h| {
            node.name
                .get(..h.len())
                .is_some_and(|p| p.eq_ignore_ascii_case(h))
        })
}

/// Hides every state but the chosen one (default: the first) at each spot,
/// and runtime helpers unless `helpers`. Hiding a node hides its subtree.
pub fn apply(
    nodes: &mut [Node],
    groups: &[StateGroup],
    chosen: &HashMap<usize, usize>,
    helpers: bool,
) {
    let mut hide = super::Bits::new(nodes.len());
    for g in groups {
        let pick = chosen
            .get(&g.members[0])
            .copied()
            .filter(|c| g.members.contains(c))
            .unwrap_or(g.members[0]);
        for &m in &g.members {
            hide.set(m, m != pick);
        }
    }
    if !helpers {
        for (i, n) in nodes.iter().enumerate() {
            if is_helper(n) {
                hide.set(i, true);
            }
        }
    }
    let mut until: Option<usize> = None;
    for i in 0..nodes.len() {
        if let Some(depth) = until {
            if nodes[i].depth > depth {
                nodes[i].visible = false;
                continue;
            }
            until = None;
        }
        if hide.get(i) {
            nodes[i].visible = false;
            until = Some(nodes[i].depth);
        }
    }
}

/// The group a node is a state of, and whether it is the shown one.
pub fn state_of(
    groups: &[StateGroup],
    chosen: &HashMap<usize, usize>,
    node: usize,
) -> Option<(usize, bool)> {
    let g = groups.iter().find(|g| g.members.contains(&node))?;
    let pick = chosen
        .get(&g.members[0])
        .copied()
        .filter(|c| g.members.contains(c))
        .unwrap_or(g.members[0]);
    Some((g.members[0], pick == node))
}

#[cfg(test)]
mod tests {
    use super::{apply, groups};
    use crate::ui::canvas::{Kind, Node};
    use std::collections::HashMap;

    fn node(name: &str, depth: usize, kind: Kind, at: [f32; 2]) -> Node {
        Node {
            widget: 0,
            foreign: false,
            item: 0,
            depth,
            name: name.into(),
            kind,
            rect: [at[0], at[1], 10., 10.],
            image: None,
            slices: None,
            text: None,
            font: None,
            text_size: 16.,
            line_height: 20.,
            align: [0; 2],
            overflow: 0,
            color: None,
            shadow: None,
            opacity: 1.,
            visible: true,
            clip: None,
            baked: false,
            sprite: None,
            runtime: false,
            owners: Default::default(),
            tree: 0,
            clip_by: usize::MAX,
        }
    }

    fn tree() -> Vec<Node> {
        vec![
            node("root", 0, Kind::Container, [0., 0.]),
            node("NoQuests", 1, Kind::Container, [0., 0.]),
            node("text", 2, Kind::Label, [0., 0.]),
            node("Tracked", 1, Kind::Container, [1., 0.]),
            node("entry", 2, Kind::Image, [1., 0.]),
            node("Blocker", 1, Kind::Container, [300., 0.]),
            node("hit", 2, Kind::Image, [300., 0.]),
            node("bg", 1, Kind::Image, [0., 0.]),
        ]
    }

    #[test]
    fn groups_at_one_spot_show_one_state_at_a_time() {
        let nodes = tree();
        let found = groups(&nodes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].members, [1, 3]);

        let mut shown = nodes.clone();
        apply(&mut shown, &found, &HashMap::new(), true);
        let visible: Vec<&str> = shown
            .iter()
            .filter(|n| n.visible)
            .map(|n| &*n.name)
            .collect();
        assert_eq!(
            visible,
            ["root", "NoQuests", "text", "Blocker", "hit", "bg"]
        );

        let mut shown = nodes.clone();
        apply(&mut shown, &found, &HashMap::from([(1, 3)]), false);
        let visible: Vec<&str> = shown
            .iter()
            .filter(|n| n.visible)
            .map(|n| &*n.name)
            .collect();
        assert_eq!(
            visible,
            ["root", "Tracked", "entry", "bg"],
            "second state, helpers hidden"
        );
    }
}
