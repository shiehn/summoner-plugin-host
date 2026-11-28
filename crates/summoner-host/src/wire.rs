//! The control channel's write side: one writer behind a lock, so frames from the reader, the message thread and
//! the mach thread never interleave.
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use summoner_proto::codec::{self, FF_ERROR, FF_EVENT, FF_REPLY};

pub type Writer = Arc<Mutex<UnixStream>>;

/// The protocol this child CLAIMS at `hello` (`major << 16 | minor`): the child's own statement of what it implements,
/// never the pinned summoner-proto's `ABI_VERSION` (a pin move alone must not make the child claim a minor whose
/// behaviour it lacks, because the host application gates on the claim). 4.0 = a slot is a `PluginRef`
/// (`SlotDesc.plugin`: a VST3 class by id, an AU entry by codes, the test double's knobs), the scan reports raw
/// `ScanClass` facts, and `hello` carries no toolkit name; every 3.x item below is part of 4.0, 3.9 and 3.10's slot
/// notices included. The host application speaks only 4.x (each side refuses the other major). 3.9 = `SlotInfo.notices`
/// and `chain.prepared`'s `slotNotices` ([`summoner_proc::Plugin::notices`]: an AU instrument that refused its inputs
/// names `inputsRefused`); nothing is gated on it (child to host application only). 3.10 adds an optional complete
/// notice snapshot to fresh-prepare completions, with explicit empty distinct from unknown. host/tests/au.rs exercises
/// changing acceptance across prepares. 3.8 = `chain.prepareFresh` (kind 25) on a render set answered
/// `chain.preparedFresh` (kind 61) after its processors were released and made again (`host::prepare_fresh`); the host
/// application sends it only at 3.8 or later. host/tests/prepare_fresh.rs ties the claim to kind 25. 3.7 =
/// `param.list`'s `pluginId`, `automatable`, `default`, `value`, `stepCount`, `label` and `order` (the parameter-tree
/// order), names cut at 1024 characters and pages within 48 KiB (`host::param_list`), and `param.set` (kind 35)
/// answered with its read-back (`host::param_set`); the host application reads the fields and sends `param.set` only at
/// 3.7 or later. host/tests/param_set.rs's `the_hello_claims_3_7_and_param_set_is_answered` ties the claim to kind 35.
/// 3.6 = `log.set` (kind 5) answered Inline on the reader (`reader::log_set`), a request's `corr` read with the host
/// application's log grammar and echoed on its answer ([`answer_reply`], [`answer_event`]), `HelloReply.wiredBytes` /
/// `unwiredBytes` and `chain.prepared`'s pair; the host application sends `log.set` and `corr` only at 3.6 or later.
/// host/tests/reader_edges.rs's `the_hello_claims_3_6_and_log_set_is_answered_inline` ties the claim to `log.set`. 3.5
/// = `auCodes` + `auVersion` in the scan, the AU entry picked by `auType` / `auSubtype` / `auManufacturer`, and no
/// `kAudioUnitProperty_OfflineRender` on a render set's AU slot; the host application sends AU slots only at 3.5 or
/// later. host/tests/au.rs's `the_hello_claims_3_6_and_a_triple_picks_the_colliding_entry` ties the claim to the AU
/// entry pick.
pub const PROTOCOL: u32 = 4 << 16;

// The claim's major is the pinned summoner-proto's, and its minor never runs ahead of the pinned codec's (at 4.0 the
// minor half holds trivially; it is kept for the first 4.x addition).
#[allow(clippy::absurd_extreme_comparisons)]
const _: () = assert!(
    PROTOCOL >> 16 == summoner_proto::layout::ABI_VERSION >> 16
        && (PROTOCOL & 0xffff) <= (summoner_proto::layout::ABI_VERSION & 0xffff)
);

pub fn writer(s: UnixStream) -> Writer {
    Arc::new(Mutex::new(s))
}

/// A failed write means the host application is gone: the reader sees EOF and exits.
pub fn send(w: &Writer, frame: Vec<u8>) {
    if let Ok(mut s) = w.lock() {
        let _ = s.write_all(&frame);
    }
}

// Every sender here serialises FIRST and never calls the codec's encoder on a size it has not checked: `encode`
// ASSERTS the caps (MAX_META, MAX_BLOB), and a panic is exit 101, which the host application reads as the plugin's
// crash.

/// Whether a frame of `meta` and `blob` fits the codec's caps.
fn fits(meta: &[u8], blob: &[u8]) -> bool {
    meta.len() <= codec::MAX_META && blob.len() <= codec::MAX_BLOB
}

/// A reply. One that cannot be framed (its meta over MAX_META, or its blob over MAX_BLOB) is answered with an ERROR
/// reply saying so, never sent: the request still gets its answer.
pub fn reply<T: serde::Serialize>(w: &Writer, kind: u16, id: u64, meta: &T, blob: &[u8]) {
    let m = serde_json::to_vec(meta).expect("meta serialises");
    if fits(&m, blob) {
        send(w, codec::encode(kind, FF_REPLY, id, &m, blob));
    } else {
        let why = format!(
            "the reply cannot be framed: meta {} bytes (MAX_META {}), blob {} bytes (MAX_BLOB {})",
            m.len(),
            codec::MAX_META,
            blob.len(),
            codec::MAX_BLOB
        );
        eprintln!("[summoner-host] kind {kind} id {id}: {why}; an error reply instead");
        error(w, kind, id, -32603, &why);
    }
}

/// An event. One that cannot be framed is DROPPED with a line on stderr (an event has no error form; every event this
/// child sends today is bounded by the host application's own input or our fixed text; `scanned` is summoner-proto's
/// encoder's frame).
pub fn event<T: serde::Serialize>(w: &Writer, kind: u16, meta: &T, blob: &[u8]) {
    let mut m = serde_json::to_vec(meta).expect("meta serialises");
    // `--cfg mutant_echo_spontaneous` (never in a normal build): every event takes the thread's corr, so an event
    // an op happens to emit is tagged; wire's `a_plain_event_never_carries_the_threads_corr` fails.
    if cfg!(mutant_echo_spontaneous)
        && let Some(c) = crate::diag::current_corr()
    {
        let _ = codec::with_corr(&mut m, &c);
    }
    if fits(&m, blob) {
        send(w, codec::encode(kind, FF_EVENT, 0, &m, blob));
    } else {
        eprintln!(
            "[summoner-host] an event of kind {kind} cannot be framed (meta {} bytes, blob {} bytes): not sent",
            m.len(),
            blob.len()
        );
    }
}

/// `ErrorReply { code, message }`'s meta, within MAX_META: a message that does not fit (it may carry a plugin's own
/// text) is cut at a UTF-8 boundary and ends `… [truncated: N bytes]`, N the bytes cut. The longest prefix that
/// fits, by a binary search over the serialised size (JSON escaping makes the size no simple function of the cut).
pub fn error_meta(code: i32, message: &str) -> Vec<u8> {
    let meta = |m: String| {
        serde_json::to_vec(&codec::ErrorReply::new(code, m)).expect("an error reply serialises")
    };
    let whole = meta(message.to_string());
    if whole.len() <= codec::MAX_META {
        return whole;
    }
    let cut = |k: usize| {
        let mut k = k.min(message.len());
        while !message.is_char_boundary(k) {
            k -= 1;
        }
        let m = format!(
            "{}… [truncated: {} bytes]",
            &message[..k],
            message.len() - k
        );
        (k, meta(m))
    };
    let (mut lo, mut hi) = (0usize, message.len());
    let mut best = cut(0).1;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let (k, m) = cut(mid);
        if m.len() <= codec::MAX_META {
            best = m;
            lo = k + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    best
}

// `scanned` is not sent here: since the pin 8cfb12d it is the CORE's encoder's frame (`codec::encode_scanned`: form A
// or B, `overLimit`, the header-only path), sent as it is by `scan_mode::send_scanned`.

pub fn error(w: &Writer, kind: u16, id: u64, code: i32, message: &str) {
    send(
        w,
        codec::encode(
            kind,
            FF_REPLY | FF_ERROR,
            id,
            &error_meta(code, message),
            &[],
        ),
    );
}

/// `meta` with the answered request's `corr` (the host application's splice; a refusal, the meta at its cap, sends it
/// without one).
fn echoed(mut m: Vec<u8>, corr: Option<&str>) -> Vec<u8> {
    if let Some(c) = corr {
        let _ = codec::with_corr(&mut m, c);
    }
    m
}

/// The reply that answers a request, carrying its `corr` (`None`: the request had none). Only the answer
/// sites call this; [`reply`] stays bare.
pub fn answer_reply<T: serde::Serialize>(
    w: &Writer,
    kind: u16,
    id: u64,
    meta: &T,
    blob: &[u8],
    corr: Option<&str>,
) {
    let m = echoed(serde_json::to_vec(meta).expect("meta serialises"), corr);
    if fits(&m, blob) {
        send(w, codec::encode(kind, FF_REPLY, id, &m, blob));
    } else {
        reply(w, kind, id, meta, blob);
    }
}

/// The completion event of a ReceiptAck request, carrying its `corr`. Spontaneous events use [`event`],
/// which never reads the thread's corr.
pub fn answer_event<T: serde::Serialize>(
    w: &Writer,
    kind: u16,
    meta: &T,
    blob: &[u8],
    corr: Option<&str>,
) {
    let m = echoed(serde_json::to_vec(meta).expect("meta serialises"), corr);
    if fits(&m, blob) {
        send(w, codec::encode(kind, FF_EVENT, 0, &m, blob));
    } else {
        event(w, kind, meta, blob);
    }
}

/// An error reply that answers a request, carrying its `corr`.
pub fn answer_error(w: &Writer, kind: u16, id: u64, code: i32, message: &str, corr: Option<&str>) {
    let m = echoed(error_meta(code, message), corr);
    send(w, codec::encode(kind, FF_REPLY | FF_ERROR, id, &m, &[]));
}

/// The corr of the request the message thread is running now (its op's `CorrScope`), for the answer sites.
pub fn op_corr() -> Option<String> {
    crate::diag::current_corr()
}

pub fn empty() -> serde_json::Value {
    serde_json::json!({})
}

/// One `recvmsg` of the control stream: the bytes read; EOF or a recv error (`n <= 0`) is `_exit(0)` (the host
/// application is gone, or closed us); a control message the kernel truncated (MSG_CTRUNC: descriptors lost; the FFI
/// crate's `InvalidData` with no OS error) is `die(4, "truncated descriptors")`.
pub fn recv_or_exit(
    sock: &UnixStream,
    buf: &mut [u8],
    got: &mut Vec<std::os::fd::OwnedFd>,
) -> usize {
    match runecraft_ipc::uds::recv_with_fds(sock, buf, got) {
        Ok(0) => summoner_sys::proc_exit::exit_now(0),
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData && e.raw_os_error().is_none() => {
            die(4, "truncated descriptors")
        }
        Err(_) => summoner_sys::proc_exit::exit_now(0),
    }
}

/// A protocol error or a broken invariant: say why on stderr and exit with `code` (the host application reads the
/// exit). `_exit`: no plugin's static destructors run (stderr is unbuffered).
pub fn die(code: i32, why: &str) -> ! {
    eprintln!("[summoner-host] {why} (exit {code})");
    summoner_sys::proc_exit::exit_now(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use summoner_proto::codec::{Direction, Frame, kind};

    /// Every frame `f` makes the writer send, decoded as the host application decodes (and checked by its own
    /// validation).
    fn sent(f: impl FnOnce(&Writer)) -> Vec<Frame> {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        // Read on a thread of its own while `f` writes: a frame near MAX_META is larger than the socket's buffer, so
        // a write with no reader blocks for ever (it did, the first time).
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            theirs.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let w = writer(ours);
        f(&w);
        drop(w);
        let bytes = reader.join().unwrap();
        let mut d = codec::Decoder::new(Direction::ToCore);
        let mut out = Vec::new();
        d.feed(&bytes, &mut |fr| out.push(fr))
            .expect("every frame decodes");
        for fr in &out {
            codec::check_to_core(fr).expect("summoner-proto's own check");
        }
        out
    }

    /// An error message carrying a plugin's own 70 KiB text (multi-byte UTF-8, quotes and control
    /// bytes, which JSON escaping widens to six bytes each) is cut to fit with its marker; never a panic.
    #[test]
    fn a_70_kib_plugin_string_in_an_error_is_truncated_to_fit_with_its_marker() {
        let unit = "plugin name é \"q\" \u{1} ";
        let big: String = unit.repeat(70 * 1024 / unit.len() + 1);
        assert!(big.len() > 70 * 1024);
        let f = sent(|w| error(w, kind::CHAIN_LOAD, 9, -32000, &big));
        assert_eq!(f.len(), 1);
        assert!(f[0].meta.len() <= codec::MAX_META, "{}", f[0].meta.len());
        assert_eq!(
            (f[0].header.id, f[0].header.flags),
            (9, FF_REPLY | FF_ERROR)
        );
        let e: codec::ErrorReply = f[0].meta_as().unwrap();
        assert_eq!(e.code, -32000);
        let (kept, marker) = e.message.rsplit_once("… [truncated: ").expect("the marker");
        let n: usize = marker.strip_suffix(" bytes]").unwrap().parse().unwrap();
        assert_eq!(kept.len() + n, big.len(), "N is the bytes cut");
        assert!(big.starts_with(kept), "a prefix, cut at a char boundary");
        // Near the cap: the longest prefix, not a timid one (one more char would not fit).
        assert!(
            f[0].meta.len() > codec::MAX_META - 64,
            "{}",
            f[0].meta.len()
        );
        // Within the cap: unchanged.
        let f = sent(|w| error(w, kind::CHAIN_LOAD, 9, -32000, "short"));
        assert_eq!(
            f[0].meta_as::<codec::ErrorReply>().unwrap().message,
            "short"
        );
    }

    /// A reply that cannot be framed is answered with an error reply saying so; an event that cannot is dropped.
    #[test]
    fn an_unframeable_reply_is_an_error_reply_and_an_unframeable_event_is_dropped() {
        let big = serde_json::json!({ "x": "y".repeat(codec::MAX_META) });
        let f = sent(|w| reply(w, kind::STATE_GET, 4, &big, &[]));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].header.flags, FF_REPLY | FF_ERROR);
        let e: codec::ErrorReply = f[0].meta_as().unwrap();
        assert!(
            e.message.starts_with("the reply cannot be framed"),
            "{}",
            e.message
        );
        let f = sent(|w| event(w, kind::CHAIN_PARKED, &big, &[]));
        assert!(f.is_empty(), "dropped, never a panic");
        let f = sent(|w| reply(w, kind::STATE_GET, 4, &empty(), &[]));
        assert_eq!(
            f[0].header.flags, FF_REPLY,
            "a reply that fits is sent as it is"
        );
    }

    /// The echo is explicit. A plain `event` inside a request's `CorrScope` carries no corr (so a spontaneous
    /// event an op emits, `latency.changed` at a prepare, is never tagged); `answer_event` / `answer_reply` /
    /// `answer_error` carry the one they are given, and a meta at its cap is sent without it. Mutation `--cfg
    /// mutant_echo_spontaneous` fails the first assert.
    #[test]
    fn a_plain_event_never_carries_the_threads_corr() {
        let f = sent(|w| {
            let _c = crate::diag::CorrScope::enter(Some("r:7"));
            event(
                w,
                kind::LATENCY_CHANGED,
                &codec::LatencyChanged::default(),
                &[],
            );
            answer_event(w, kind::CHAIN_PARKED, &empty(), &[], op_corr().as_deref());
            answer_reply(
                w,
                kind::LOG_SET,
                5,
                &codec::LogSetReply::new(3),
                &[],
                Some("r:8"),
            );
            answer_error(w, kind::LOG_SET, 6, -32602, "no", Some("r:9"));
            answer_event(w, kind::CHAIN_UNLOADED, &empty(), &[], None);
        });
        let corr: Vec<codec::CorrRead> = f.iter().map(|x| codec::corr_of_meta(&x.meta)).collect();
        assert_eq!(corr[0], codec::CorrRead::Absent, "a spontaneous event");
        assert_eq!(corr[1], codec::CorrRead::Valid("r:7".into()));
        assert_eq!(corr[2], codec::CorrRead::Valid("r:8".into()));
        assert_eq!(corr[3], codec::CorrRead::Valid("r:9".into()));
        assert_eq!(corr[4], codec::CorrRead::Absent);
        assert!(f[3].header.is_error());
        // At the cap: sent without the key, still framed.
        let big = serde_json::json!({ "p": "x".repeat(codec::MAX_META - 8) });
        let f = sent(|w| answer_reply(w, kind::STATE_GET, 7, &big, &[], Some("r:1")));
        assert_eq!(f.len(), 1);
        assert_eq!(codec::corr_of_meta(&f[0].meta), codec::CorrRead::Absent);
    }
}
