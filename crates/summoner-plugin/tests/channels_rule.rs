//! The slot channel rule (`summoner_plugin::channels`), black box: the whole truth table as data, and a seeded
//! digest of the rule over generated blocks (bit for bit; recorded once, so any change of the arithmetic shows).

use summoner_plugin::channels::{channel_rule_after, channel_rule_before};

/// A clean-room pin (the slot channel rule's behaviour specification): the whole truth table, as data, for every
/// bus shape in {0, 1, 2, 3, 4, 6, 8} each way. Columns: inputs, outputs, what the plugin sees on channels 0 and 1, what the
/// next slot gets after a plugin that writes nothing, and after one that writes 5 to channel 0 and 7 to channel 1
/// (each only when it has that output). Symbols: L and R the block's channels, A = (L + R) * 0.5 in f32, Z zero.
const TABLE: [&str; 49] = [
    "0 0 ZZ ZZ ZZ",
    "0 1 ZZ ZZ 55",
    "0 2 ZZ ZZ 57",
    "0 3 ZZ ZZ 57",
    "0 4 ZZ ZZ 57",
    "0 6 ZZ ZZ 57",
    "0 8 ZZ ZZ 57",
    "1 0 AZ ZZ ZZ",
    "1 1 AZ AA 55",
    "1 2 LZ LZ 57",
    "1 3 AZ AZ 57",
    "1 4 AZ AZ 57",
    "1 6 AZ AZ 57",
    "1 8 AZ AZ 57",
    "2 0 LR ZZ ZZ",
    "2 1 LR LZ 5Z",
    "2 2 LR LR 57",
    "2 3 LR LR 57",
    "2 4 LR LR 57",
    "2 6 LR LR 57",
    "2 8 LR LR 57",
    "3 0 LR ZZ ZZ",
    "3 1 LR LZ 5Z",
    "3 2 LR LR 57",
    "3 3 LR LR 57",
    "3 4 LR LR 57",
    "3 6 LR LR 57",
    "3 8 LR LR 57",
    "4 0 LR ZZ ZZ",
    "4 1 LR LZ 5Z",
    "4 2 LR LR 57",
    "4 3 LR LR 57",
    "4 4 LR LR 57",
    "4 6 LR LR 57",
    "4 8 LR LR 57",
    "6 0 LR ZZ ZZ",
    "6 1 LR LZ 5Z",
    "6 2 LR LR 57",
    "6 3 LR LR 57",
    "6 4 LR LR 57",
    "6 6 LR LR 57",
    "6 8 LR LR 57",
    "8 0 LR ZZ ZZ",
    "8 1 LR LZ 5Z",
    "8 2 LR LR 57",
    "8 3 LR LR 57",
    "8 4 LR LR 57",
    "8 6 LR LR 57",
    "8 8 LR LR 57",
];

fn sym(c: u8, l: f32, r: f32) -> f32 {
    match c {
        b'L' => l,
        b'R' => r,
        b'A' => (l + r) * 0.5,
        b'Z' => 0.0,
        b'5' => 5.0,
        b'7' => 7.0,
        _ => unreachable!(),
    }
}

#[test]
fn the_rule_is_the_specifications_truth_table() {
    let (l, r) = (0.75f32, -0.3f32);
    for row in TABLE {
        let f: Vec<&str> = row.split(' ').collect();
        let (i, o): (u32, u32) = (f[0].parse().unwrap(), f[1].parse().unwrap());
        let want = |k: usize| [sym(f[k].as_bytes()[0], l, r), sym(f[k].as_bytes()[1], l, r)];
        let mut b = [vec![l; 3], vec![r; 3]];
        channel_rule_before(i, o, &mut b, 3);
        let seen = [b[0][0], b[1][0]];
        assert_eq!(
            seen.map(f32::to_bits),
            want(2).map(f32::to_bits),
            "{row}: seen"
        );
        let mut id = b.clone();
        channel_rule_after(i, o, &mut id, 3);
        assert_eq!(
            [id[0][2], id[1][2]].map(f32::to_bits),
            want(3).map(f32::to_bits),
            "{row}: identity"
        );
        let mut w = b.clone();
        for (c, v) in [5.0f32, 7.0]
            .into_iter()
            .enumerate()
            .take(o.min(2) as usize)
        {
            w[c].fill(v);
        }
        channel_rule_after(i, o, &mut w, 3);
        assert_eq!(
            [w[0][1], w[1][1]].map(f32::to_bits),
            want(4).map(f32::to_bits),
            "{row}: writer"
        );
    }
}

/// SplitMix64 (the specification's generator).
fn splitmix(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A sample: mostly finite values of any exponent, plus the edges (both zeros, both infinities, subnormals); never
/// a NaN (its payload is the platform's, so it stays out of a digest).
fn sample(s: &mut u64) -> f32 {
    let v = splitmix(s);
    match v % 8 {
        0 => [
            0.0,
            -0.0,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::MIN_POSITIVE / 4.0,
            -f32::MIN_POSITIVE / 3.0,
            f32::MAX,
            -f32::MAX,
        ][(v >> 8) as usize % 8],
        1 => ((v >> 8) as u32 & 0x007F_FFFF) as f32 * f32::EPSILON,
        _ => {
            let x = f32::from_bits((v >> 16) as u32);
            if x.is_nan() { 1.5 } else { x }
        }
    }
}

/// 2048 generated blocks (seed 0x5EED_C018): shape from {0, 1, 2, 3, 4, 6, 8} each way, 6-frame channels, `frames`
/// 0..=6, the rule before and after a plugin that writes nothing. The FNV-1a 64 digest covers every sample's bits
/// of both channels after the before step and after the after step, the untouched tail included (a NaN as
/// 0x7FC0_0000).
#[test]
fn the_seeded_digest_is_todays() {
    const SHAPES: [u32; 7] = [0, 1, 2, 3, 4, 6, 8];
    let mut s = 0x5EED_C018u64;
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    let eat = |h: &mut u64, b: &[Vec<f32>; 2]| {
        for x in b.iter().flatten() {
            // inf + -inf is a NaN whose bits are the platform's: one canonical NaN in the digest.
            let bits = if x.is_nan() { 0x7FC0_0000 } else { x.to_bits() };
            for byte in bits.to_le_bytes() {
                *h ^= u64::from(byte);
                *h = h.wrapping_mul(0x0000_0100_0000_01B3);
            }
        }
    };
    for _ in 0..2048 {
        let i = SHAPES[(splitmix(&mut s) % 7) as usize];
        let o = SHAPES[(splitmix(&mut s) % 7) as usize];
        let frames = (splitmix(&mut s) % 7) as usize;
        let mut b = [
            (0..6).map(|_| sample(&mut s)).collect::<Vec<f32>>(),
            Vec::new(),
        ];
        b[1] = (0..6).map(|_| sample(&mut s)).collect();
        channel_rule_before(i, o, &mut b, frames);
        eat(&mut h, &b);
        channel_rule_after(i, o, &mut b, frames);
        eat(&mut h, &b);
    }
    assert_eq!(h, 0xAA93032B0F163152, "digest (seeded blocks)");
}
