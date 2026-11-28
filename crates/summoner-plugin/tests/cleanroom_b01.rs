//! Clean-room pins for the flattened parameter-tree order (`params::vst3_unit_order`), black box: only the crate's
//! public interface is used. They pin today's answers so a rewrite made from the behaviour specification alone can be
//! held to them: hand-worked cases for the edges the unit tests in `params.rs` leave open, and one seeded digest over
//! many generated inputs (the generator is stated in the specification, so a rewrite's own test can reproduce it).

use summoner_plugin::params::{NOT_IN_TREE, vst3_unit_order};

const X: u32 = NOT_IN_TREE;

#[test]
fn no_parameters_give_an_empty_order() {
    assert_eq!(vst3_unit_order(&[], &[]), Vec::<u32>::new());
    assert_eq!(vst3_unit_order(&[(1, 0), (2, 1)], &[]), Vec::<u32>::new());
}

#[test]
fn a_unit_that_is_its_own_parent_is_out_of_the_tree() {
    assert_eq!(vst3_unit_order(&[(1, 1)], &[1, 0, 1]), vec![X, 0, X]);
}

#[test]
fn a_chain_that_runs_into_a_cycle_is_out_of_the_tree() {
    // 3 -> 1 -> 2 -> 1: unit 3 is not in the cycle but only reaches it.
    assert_eq!(
        vst3_unit_order(&[(1, 2), (2, 1), (3, 1)], &[3, 0, 2]),
        vec![X, 0, X]
    );
    // A later entry for unit 1 closes a cycle with 2.
    assert_eq!(
        vst3_unit_order(&[(1, 0), (2, 1), (1, 2)], &[2, 0]),
        vec![X, 0]
    );
}

#[test]
fn a_unit_whose_parent_has_no_entry_hangs_from_the_root() {
    // Root children: p0, u5 (first used by p1), p2; u5 holds p1, p3.
    assert_eq!(vst3_unit_order(&[(5, 77)], &[0, 5, 0, 5]), vec![0, 1, 3, 2]);
}

#[test]
fn zeroed_unit_entries_and_an_entry_for_the_root_change_nothing() {
    assert_eq!(
        vst3_unit_order(&[(0, 0), (1, 0), (0, 0)], &[1, 0, 1]),
        vec![0, 2, 1]
    );
    // An entry that gives unit 0 a parent: the root stays the root.
    assert_eq!(vst3_unit_order(&[(0, 3), (3, 0)], &[0, 3]), vec![0, 1]);
}

#[test]
fn negative_unit_ids_are_ordinary_units() {
    // Root: [u-1, p1]; u-1: [u-2, p2]; u-2: [p0].
    assert_eq!(
        vst3_unit_order(&[(-1, 0), (-2, -1)], &[-2, 0, -1]),
        vec![0, 2, 1]
    );
}

#[test]
fn siblings_sit_in_the_order_of_their_first_use() {
    // Root: [u1, p1]; u1: [u2 (p0), u3 (p2), p3].
    assert_eq!(
        vst3_unit_order(&[(1, 0), (2, 1), (3, 1)], &[2, 0, 3, 1]),
        vec![0, 3, 1, 2]
    );
}

#[test]
fn the_unit_list_order_does_not_matter_only_the_last_entry_per_id() {
    assert_eq!(vst3_unit_order(&[(2, 1), (1, 0)], &[2, 1]), vec![0, 1]);
    assert_eq!(vst3_unit_order(&[(1, 0), (2, 1)], &[2, 1]), vec![0, 1]);
}

#[test]
fn a_wide_flat_tree_keeps_the_index_order() {
    let n = 100_000;
    let units: Vec<(i32, i32)> = (1..=n).map(|u| (u, 0)).collect();
    let params: Vec<i32> = (1..=n).collect();
    let want: Vec<u32> = (0..n as u32).collect();
    assert_eq!(vst3_unit_order(&units, &params), want);
}

/// SplitMix64, the generator the specification names.
struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// An id in -2..=6.
    fn id(&mut self) -> i32 {
        (self.next() % 9) as i32 - 2
    }
}

/// FNV-1a, 64 bits.
fn fnv(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= u64::from(*b);
        *h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

/// 4096 generated cases (seed 0x5EED_B001): up to 8 unit entries and up to 12 parameters, ids -2..=6. The digest
/// covers each case's length and every `order` value, little-endian.
#[test]
fn the_seeded_digest_is_todays() {
    let mut r = SplitMix(0x5EED_B001);
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    let mut out_of_tree = 0usize;
    for _ in 0..4096 {
        let n_units = (r.next() % 9) as usize;
        let units: Vec<(i32, i32)> = (0..n_units).map(|_| (r.id(), r.id())).collect();
        let n_params = (r.next() % 13) as usize;
        let params: Vec<i32> = (0..n_params).map(|_| r.id()).collect();
        let order = vst3_unit_order(&units, &params);
        assert_eq!(order.len(), n_params);
        fnv(&mut h, &(n_params as u32).to_le_bytes());
        for o in &order {
            fnv(&mut h, &o.to_le_bytes());
        }
        out_of_tree += order.iter().filter(|o| **o == X).count();
        // In the tree: the values are 0..k, each once.
        let mut inside: Vec<u32> = order.iter().copied().filter(|o| *o != X).collect();
        inside.sort_unstable();
        assert!(
            inside.iter().enumerate().all(|(i, o)| *o == i as u32),
            "{units:?} {params:?} {order:?}"
        );
    }
    assert!(out_of_tree > 0, "the generator reaches cycles");
    assert_eq!(h, 0xEF49B47B97A13E39, "digest (seeded cases)");
}
