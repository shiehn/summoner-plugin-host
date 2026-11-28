//! The layer's own checks; runecraft-log's behaviour (the files, rotation, caps, the drainer, the disk rule) is tested
//! there.
use super::*;

/// The name hash's vectors (Python's independent `hmac`, salt 00..0f): the child's names hash as its parent's.
#[test]
fn names_hash_with_the_published_vectors() {
    let salt: [u8; 16] = std::array::from_fn(|i| i as u8);
    assert_eq!(hash_keyed(&salt, "My Secret Song"), "h:72a9021a");
    assert_eq!(hash_keyed(&salt, "Summoner Lab Gain"), "h:b891153c");
    assert_eq!(
        parse_salt("000102030405060708090a0b0c0d0e0f"),
        Some(salt),
        "the salt variable: exactly 32 hex"
    );
    assert_eq!(parse_salt("0001"), None);
}

/// The grammar, all or nothing: a refused spec leaves the default and says why; a partial spec of the host's own words
/// keeps the host's vocabulary.
#[test]
fn a_spec_applies_whole_or_not_at_all() {
    let (v, w, warn) = parse("all=error,audio=trace");
    assert!(warn.is_empty());
    assert!(std::ptr::eq(v, &HOST_VOCAB));
    assert_eq!(runecraft_log::level_of(w, area::AUDIO), Level::Trace);
    assert_eq!(runecraft_log::level_of(w, area::HOST), Level::Error);
    let (_, w, warn) = parse("audio=trace,nope=info");
    assert_eq!(w, DEFAULT_LEVELS, "one bad item refuses the whole spec");
    assert_eq!(warn.len(), 1, "{warn:?}");
    assert_eq!(parse("").1, DEFAULT_LEVELS);
    assert_eq!(parse("debug").1, runecraft_log::uniform(Level::Debug));
}

/// The parent's complete list is adopted (its words, nibble for nibble); one whose pinned words differ is not, with a
/// warning, and is then refused whole against the host's words.
#[test]
fn the_parents_complete_spec_names_the_areas() {
    let parent = "a0=warn,control=warn,a2=warn,a3=warn,audio=warn,a5=warn,a6=warn,a7=warn,a8=warn,a9=warn,\
a10=warn,host=trace,plugin=info,a13=warn,a14=warn,a15=warn";
    let (v, w, warn) = parse(parent);
    assert!(warn.is_empty(), "{warn:?}");
    assert_eq!(v.areas[0], "a0");
    assert_eq!(runecraft_log::describe(v, w), parent);
    let (v, w, warn) = parse(&parent.replace(",host=", ",hosts="));
    assert!(std::ptr::eq(v, &HOST_VOCAB));
    assert_eq!(w, DEFAULT_LEVELS);
    assert_eq!(warn.len(), 2, "the adoption's and the parse's: {warn:?}");
}

/// A complete list refused for any reason but its pinned words gets no adoption warning; only
/// the parse's, when the parse refuses it too. Mutant `mutant_diag_warns_any_refusal` (every refused 16-item list
/// warned) turns this red.
#[test]
fn only_a_pinned_word_mismatch_warns_about_adoption() {
    let parent = "a0=warn,control=warn,a2=warn,a3=warn,audio=warn,a5=warn,a6=warn,a7=warn,a8=warn,a9=warn,\
a10=warn,host=trace,plugin=info,a13=warn,a14=warn,a15=warn";
    let adoption = |w: &[String]| {
        w.iter()
            .filter(|w| w.contains("(this child's own words apply)"))
            .count()
    };
    // Refused for adoption at a non-pinned item (`all`, a bare level, a duplicate word): the pinned words are the
    // host's, so no adoption warning; the parse against the host's words refuses it (`a2` is not one), with its own.
    for spec in [
        parent.replace("a0=warn,", "all=warn,"),
        parent.replace("a5=warn,", "warn,"),
        parent.replace("a6=warn,", "a5=warn,"),
    ] {
        let (v, w, warn) = parse(&spec);
        assert!(std::ptr::eq(v, &HOST_VOCAB), "{spec}");
        assert_eq!(w, DEFAULT_LEVELS, "{spec}");
        assert_eq!(adoption(&warn), 0, "{spec}: {warn:?}");
        assert_eq!(warn.len(), 1, "the parse's only: {spec}: {warn:?}");
    }
    // A pinned word changed, or missing, at each pinned nibble: the adoption's warning, then the parse's.
    for (from, to) in [
        (",control=", ",ctl="),
        (",audio=", ",sound="),
        (",host=", ",hosts="),
        (",plugin=info", ",info"),
    ] {
        let spec = parent.replace(from, to);
        let (v, _, warn) = parse(&spec);
        assert!(std::ptr::eq(v, &HOST_VOCAB), "{spec}");
        assert_eq!(adoption(&warn), 1, "{spec}: {warn:?}");
    }
    // A short list is never the parent's: no adoption warning whatever its words.
    assert_eq!(adoption(&parse("hosts=trace,plugin=info").2), 0);
}

/// A ring without a drainer is empty and costs nothing (no drainer runs in this test process).
#[test]
fn a_ring_without_a_drainer_is_a_no_op() {
    let mut r = rt_ring("test", 7);
    assert!(r.ring.is_none());
    r.block(1, 64, 1);
    r.slow(9, 1, 64, 2);
}
