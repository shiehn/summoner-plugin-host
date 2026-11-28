//! Protocol 3.7: the order a child reports in
//! `ParamEntry.order`, the plugin's parameter tree flattened depth first, and the
//! name cut. The function is pure and
//! has no recursion, so a hostile plugin's units cannot overflow a stack. The plugin-host child computes `order` with it,
//! and so does the host application's fake host.
//!
//! The flat list is a depth-first walk of each group's children in insertion order: for a VST3 the groups are its
//! `IUnitInfo` units, for an AU its `kAudioUnitParameterFlag_HasClump` clumps.
// A child's message thread and the host application's control thread only, at a load or a list: never a realtime path.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;

/// `order` of a parameter that is not in the flat list (a VST3 unit cycle leaves its groups out of the root's walk).
pub const NOT_IN_TREE: u32 = u32::MAX;

/// Rev 2 C3: a name is cut at this many characters.
pub const NAME_CHARS: usize = 1024;

/// A `param.list` reply's meta stays at or under this many bytes (`MAX_META` is 64 KiB).
pub const PAGE_BYTES: usize = 48 * 1024;

/// Rev 2 C5: a label is cut at this many characters.
pub const LABEL_CHARS: usize = 64;

/// The VST3 root unit (`Vst::kRootUnitId`).
pub const VST3_ROOT_UNIT: i32 = 0;

/// `s` cut to at most `max` characters (Unicode scalar values), at a character boundary.
pub fn cut_chars(mut s: String, max: usize) -> String {
    if let Some((at, _)) = s.char_indices().nth(max) {
        s.truncate(at);
    }
    s
}

/// Whether a unit's ancestor walk reaches the root.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    Walking,
    Root,
    Cycle,
}

/// One child of a tree node; children are sorted by their first-use index.
#[derive(Clone, Copy)]
enum Child {
    Param(usize),
    Unit(usize),
}

/// `units`: the plugin's unit entries as `(unit id, parent unit id)`, in the order the host read them.
/// `param_units[i]`: parameter index `i`'s unit id.
/// Returns one value per parameter: its position in the flattened tree, or `NOT_IN_TREE`.
pub fn vst3_unit_order(units: &[(i32, i32)], param_units: &[i32]) -> Vec<u32> {
    // Parent of each non-root unit; the last entry for an id wins; entries for the root are ignored.
    // Ordered maps throughout: O(log n) per operation in the worst case, whatever the ids.
    let mut parent: BTreeMap<i32, i32> = BTreeMap::new();
    for &(id, p) in units {
        if id != VST3_ROOT_UNIT {
            parent.insert(id, p);
        }
    }
    // A unit id that is the root or has no entry ends a walk at the root.
    let is_terminal = |u: i32| u == VST3_ROOT_UNIT || !parent.contains_key(&u);

    // Memoised, iterative ancestor walks: every unit is resolved once.
    let mut reach: BTreeMap<i32, Reach> = BTreeMap::new();
    let mut path: Vec<i32> = Vec::new();
    let mut in_tree = vec![false; param_units.len()];
    for (i, &u) in param_units.iter().enumerate() {
        if is_terminal(u) {
            in_tree[i] = true;
            continue;
        }
        path.clear();
        let mut cur = u;
        let result = loop {
            if is_terminal(cur) {
                break Reach::Root;
            }
            match reach.get(&cur).copied() {
                Some(Reach::Walking) => break Reach::Cycle,
                Some(done) => break done,
                None => {
                    reach.insert(cur, Reach::Walking);
                    path.push(cur);
                    cur = parent[&cur];
                }
            }
        };
        for &v in &path {
            reach.insert(v, result);
        }
        in_tree[i] = result == Reach::Root;
    }

    // Tree nodes: 0 is the root, 1.. are unit nodes in first-use order. A unit's first use is the
    // first in-tree parameter whose walk passes it; walks stop at an already-marked unit because
    // its ancestors are marked by an earlier (smaller) index.
    let mut node_of: BTreeMap<i32, usize> = BTreeMap::new();
    let mut node_units: Vec<i32> = Vec::new(); // node_units[n - 1] = unit id of node n
    let mut node_first: Vec<usize> = Vec::new(); // node_first[n - 1] = first-use index of node n
    let mut children: Vec<Vec<(usize, Child)>> = vec![Vec::new()];
    for (i, &u) in param_units.iter().enumerate() {
        if !in_tree[i] {
            continue;
        }
        let mut cur = u;
        while !is_terminal(cur) && !node_of.contains_key(&cur) {
            node_units.push(cur);
            node_first.push(i);
            children.push(Vec::new());
            node_of.insert(cur, node_units.len());
            cur = parent[&cur];
        }
        let owner = node_of.get(&u).copied().unwrap_or(0);
        children[owner].push((i, Child::Param(i)));
    }
    for (k, &g) in node_units.iter().enumerate() {
        let n = k + 1;
        let owner = node_of.get(&parent[&g]).copied().unwrap_or(0);
        children[owner].push((node_first[k], Child::Unit(n)));
    }
    for list in &mut children {
        list.sort_unstable_by_key(|c| c.0);
    }

    // Iterative pre-order depth-first visit, counting parameters only.
    let mut order = vec![NOT_IN_TREE; param_units.len()];
    let mut next: u32 = 0;
    let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
    while let Some(top) = stack.last_mut() {
        let (n, k) = *top;
        if let Some(&(_, child)) = children[n].get(k) {
            top.1 += 1;
            match child {
                Child::Param(i) => {
                    order[i] = next;
                    next += 1;
                }
                Child::Unit(m) => stack.push((m, 0)),
            }
        } else {
            stack.pop();
        }
    }
    order
}

/// An AU's order, by its clumps: the valid `kAudioUnitProperty_ParameterList`
/// entries in order, `clumps[i]` the clump id when `kAudioUnitParameterFlag_HasClump` is set. A clump's group is made
/// and appended to the root at its first parameter. Every other parameter is appended to the root.
pub fn au_clump_order(clumps: &[Option<u32>]) -> Vec<u32> {
    // The root's children: a parameter, or a clump (its group, by first occurrence).
    let mut root: Vec<Result<usize, u32>> = Vec::new();
    let mut groups: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (i, c) in clumps.iter().enumerate() {
        match c {
            None => root.push(Ok(i)),
            Some(c) => {
                if !groups.contains_key(c) {
                    root.push(Err(*c));
                }
                groups.entry(*c).or_default().push(i);
            }
        }
    }
    let mut order = vec![NOT_IN_TREE; clumps.len()];
    let mut n = 0u32;
    for x in root {
        match x {
            Ok(i) => {
                order[i] = n;
                n += 1;
            }
            Err(c) => {
                for &i in groups.get(&c).map_or(&[][..], |v| v.as_slice()) {
                    order[i] = n;
                    n += 1;
                }
            }
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plugin_without_units_keeps_its_index_order() {
        assert_eq!(vst3_unit_order(&[], &[0, 0, 0]), vec![0, 1, 2]);
        assert_eq!(au_clump_order(&[None, None]), vec![0, 1]);
    }

    #[test]
    fn vst3_groups_sit_at_their_first_parameter_and_gather_their_members() {
        // Units: 1 (root's), 2 (root's), 3 (child of 1).
        let units = [(1, 0), (2, 0), (3, 1)];
        // Params: 0 in 1, 1 in root, 2 in 2, 3 in 1, 4 in 3, 5 in root, 6 in 2.
        let order = vst3_unit_order(&units, &[1, 0, 2, 1, 3, 0, 2]);
        // Root: [G1, p1, G2, p5]; G1: [p0, p3, G3]; G3: [p4]; G2: [p2, p6].
        // Flat: p0, p3, p4, p1, p2, p6, p5.
        assert_eq!(order, vec![0, 3, 4, 1, 2, 6, 5]);
    }

    #[test]
    fn vst3_ancestors_are_made_first_and_attached_in_unwinding_order() {
        // 5 → 4 → 0: the first parameter is in 5, so 4 joins the root, then 5 joins 4.
        let order = vst3_unit_order(&[(4, 0), (5, 4)], &[5, 4, 0]);
        // Root: [G4, p2]; G4: [G5, p1]; G5: [p0]. Flat: p0, p1, p2.
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn an_unknown_unit_is_the_root_and_a_later_unit_id_replaces_an_earlier_one() {
        // Unit 9 is unknown: root. Unit 1 is listed twice; the later parent (2) wins.
        let order = vst3_unit_order(&[(1, 0), (2, 0), (1, 2)], &[9, 1, 0]);
        // p0 → root. p1 → G1 (parent G2, made first: root ← G2, G2 ← G1). Root: [p0, G2, p2]; G2: [G1]; G1: [p1].
        assert_eq!(order, vec![0, 1, 2]);
        let order = vst3_unit_order(&[(1, 0), (2, 0), (1, 2)], &[0, 2, 1]);
        // Root: [p0, G2]; G2: [p1, G1]; G1: [p2]. Flat: p0, p1, p2.
        assert_eq!(order, vec![0, 1, 2]);
        let order = vst3_unit_order(&[(1, 0), (2, 0), (1, 2)], &[1, 0, 2]);
        // Root: [G2, p1]; G2: [G1, p2]; G1: [p0]. Flat: p0, p2, p1.
        assert_eq!(order, vec![0, 2, 1]);
    }

    #[test]
    fn a_unit_cycle_leaves_its_parameters_out_of_the_tree() {
        // 1's parent is 2, 2's parent is 1: neither reaches the root.
        let order = vst3_unit_order(&[(1, 2), (2, 1)], &[0, 1, 2, 0]);
        assert_eq!(order, vec![0, NOT_IN_TREE, NOT_IN_TREE, 1]);
    }

    #[test]
    fn a_long_unit_chain_needs_no_stack() {
        let n = 200_000;
        let units: Vec<(i32, i32)> = (1..=n).map(|u| (u, u - 1)).collect();
        let order = vst3_unit_order(&units, &[n, 0]);
        assert_eq!(order, vec![0, 1]);
    }

    #[test]
    fn au_clumps_gather_at_their_first_member() {
        // p0 clump 7, p1 none, p2 clump 3, p3 clump 7, p4 none, p5 clump 3.
        let order = au_clump_order(&[Some(7), None, Some(3), Some(7), None, Some(3)]);
        // Root: [C7 (p0, p3), p1, C3 (p2, p5), p4]. Flat: p0, p3, p1, p2, p5, p4.
        assert_eq!(order, vec![0, 2, 3, 1, 5, 4]);
    }

    #[test]
    fn names_are_cut_at_characters_not_bytes() {
        assert_eq!(cut_chars("gain".into(), 1024), "gain");
        let long: String = "é".repeat(1500);
        let cut = cut_chars(long, NAME_CHARS);
        assert_eq!(cut.chars().count(), 1024);
        assert_eq!(cut.len(), 2048);
    }
}

#[cfg(test)]
mod vst3_unit_order_tests {
    use super::*;

    const X: u32 = NOT_IN_TREE;

    #[test]
    #[allow(clippy::type_complexity)] // integration: lint only (the writer's table literal)
    fn rule_table() {
        let cases: &[(&[(i32, i32)], &[i32], &[u32])] = &[
            (&[], &[0, 0, 0], &[0, 1, 2]),
            (
                &[(1, 0), (2, 0), (3, 1)],
                &[1, 0, 2, 1, 3, 0, 2],
                &[0, 3, 4, 1, 2, 6, 5],
            ),
            (&[(4, 0), (5, 4)], &[5, 4, 0], &[0, 1, 2]),
            (&[(1, 0), (2, 0), (1, 2)], &[9, 1, 0], &[0, 1, 2]),
            (&[(1, 0), (2, 0), (1, 2)], &[1, 0, 2], &[0, 2, 1]),
            (&[(1, 2), (2, 1)], &[0, 1, 2, 0], &[0, X, X, 1]),
            (&[(1, 1)], &[1, 0, 1], &[X, 0, X]),
            (&[(1, 2), (2, 1), (3, 1)], &[3, 0, 2], &[X, 0, X]),
            (&[(5, 77)], &[0, 5, 0, 5], &[0, 1, 3, 2]),
            (&[(0, 0), (1, 0), (0, 0)], &[1, 0, 1], &[0, 2, 1]),
            (&[(0, 3), (3, 0)], &[0, 3], &[0, 1]),
            (&[(-1, 0), (-2, -1)], &[-2, 0, -1], &[0, 2, 1]),
            (&[(1, 0), (2, 1), (3, 1)], &[2, 0, 3, 1], &[0, 3, 1, 2]),
            (&[(2, 1), (1, 0)], &[2, 1], &[0, 1]),
        ];
        for (units, params, want) in cases {
            assert_eq!(
                vst3_unit_order(units, params),
                want.to_vec(),
                "units {units:?} params {params:?}"
            );
        }
    }

    #[test]
    fn deep_chain_on_default_stack() {
        let units: Vec<(i32, i32)> = (1..=200_000).map(|u| (u, u - 1)).collect();
        assert_eq!(vst3_unit_order(&units, &[200_000, 0]), vec![0, 1]);
    }

    #[test]
    fn flat_tree_of_many_units() {
        let units: Vec<(i32, i32)> = (1..=100_000).map(|u| (u, 0)).collect();
        let params: Vec<i32> = (1..=100_000).rev().collect();
        let got = vst3_unit_order(&units, &params);
        assert_eq!(got, (0..100_000u32).collect::<Vec<_>>());
    }

    #[test]
    fn extreme_ids_do_not_panic() {
        let units = [(i32::MIN, i32::MAX), (i32::MAX, i32::MIN), (-1, -1)];
        let got = vst3_unit_order(&units, &[i32::MIN, i32::MAX, -1, 0, 7]);
        assert_eq!(got, vec![X, X, X, 0, 1]);
    }
}
