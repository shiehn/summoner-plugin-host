//! The host child's side of the opt-in diagnostic log, over runecraft-log: the level switch, the grammar of
//! the launch variable, the name hash, the record schema, the drainer, the files and the caps are all runecraft-log's;
//! the words are the host's vocabulary (`summoner_proto::log_vocab`), adopted from the parent's spec when it passes
//! one. What stays here is the child's own shape:
//!
//! - [`parse`]: the launch spec, adopted ([`runecraft_log::adopt_areas`] with the host's pinned words) and parsed;
//! - [`init`]: the drainer for `comp: "host"` in the `--log-dir` folder the parent passes (resolved and held by the
//!   caller), the launch levels, the salt; warnings and errors echoed to stderr only without a folder (with one, the
//!   parent's stderr pipe would see them twice); the crash handler's `diag <file name> seq <n>` line fed from it;
//!   `session.end` written before every `_exit` (`proc_exit`'s hook);
//! - [`log`]: the child's non-realtime records, with `chain` / `slot` in `fields`;
//! - [`RtLog`]: a chain thread's realtime ring, in the host's rows (`chain.block`, trace; `chain.slow`, info: off by
//!   default), each carrying the chain's key.

#[cfg(test)]
use runecraft_log::parse_salt;
pub use runecraft_log::{
    CorrScope, DEFAULT_LEVELS, Level, Value, current_corr, enabled, installed, levels, set_levels,
};
use summoner_proto::log_vocab::{HOST_VOCAB, PINNED, code};

use runecraft_log::{Area, Vocab};
use std::os::fd::OwnedFd;
use std::path::PathBuf;

/// The host's own areas, by name.
pub mod area {
    #[cfg(test)]
    pub use summoner_proto::log_vocab::AUDIO;
    pub use summoner_proto::log_vocab::{HOST, PLUGIN};
}

/// A plugin name or a bundle path as `h:<8 hex>` (HMAC-SHA256 under the salt in effect).
pub fn hashed(text: &str) -> String {
    runecraft_log::hash_name(text)
}

/// [`hashed`] under an explicit salt (the vectors).
#[cfg(test)]
pub fn hash_keyed(salt: &[u8; 16], text: &str) -> String {
    runecraft_log::hash_keyed(salt, text)
}

/// The launch spec: the vocabulary (the parent's words when `spec` is the parent's complete list and its pinned words
/// are the host's; else the host's own), the levels over the default (all or nothing), and the warnings (a refused
/// spec leaves the default). The variable is not a flag: never an exit.
pub fn parse(spec: &str) -> (&'static Vocab, u64, Vec<String>) {
    let mut warnings = Vec::new();
    let vocab = match runecraft_log::adopt_areas(spec, &HOST_VOCAB, &PINNED) {
        Ok(v) => v,
        Err(e) => {
            // the one refusal worth its own warning is a parent's list whose pinned words are not
            // the host's. Any other refused adoption (a partial, hand-written or malformed spec) parses against the
            // host's words, and that parse says why if it refuses.
            let warn = if cfg!(mutant_diag_warns_any_refusal) {
                spec.split(',').count() == 16
            } else {
                pinned_words_differ(spec)
            };
            if warn {
                warnings.push(format!(
                    "{}: {e} (this child's own words apply)",
                    HOST_VOCAB.env_levels
                ));
            }
            &HOST_VOCAB
        }
    };
    match runecraft_log::parse_levels(vocab, spec, DEFAULT_LEVELS) {
        Ok(w) => (vocab, w, warnings),
        Err(e) => {
            warnings.push(format!(
                "{}: {e} (the default applies)",
                HOST_VOCAB.env_levels
            ));
            (vocab, DEFAULT_LEVELS, warnings)
        }
    }
}

/// A complete 16-item list (the parent's shape) that names another word than the host's at a pinned nibble (1, 4, 11,
/// 12): an item's word is its text before `=`, trimmed as `adopt_areas` trims it (the whole item when it has no `=`).
fn pinned_words_differ(spec: &str) -> bool {
    let items: Vec<&str> = spec.split(',').collect();
    items.len() == 16
        && PINNED.iter().any(|&n| {
            let item = items[n as usize].trim();
            let word = item.split_once('=').map_or(item, |(w, _)| w).trim();
            word != HOST_VOCAB.areas[n as usize]
        })
}

/// One non-realtime record, if its area's level is on. `corr` is the request's id where the child knows one.
pub fn log(
    level: Level,
    area: Area,
    corr: Option<String>,
    msg: &'static str,
    fields: Vec<(&'static str, Value)>,
) {
    if !enabled(area, level) {
        return;
    }
    let _scope = corr.as_deref().map(|c| CorrScope::enter(Some(c)));
    runecraft_log::emit(area, level, msg, fields);
}

/// The child's drainer: `comp: "host"`, in `dir` (the `--log-dir` folder, held open; `None`: warnings and errors to
/// stderr), at `levels`, keyed by `salt` (32 hex digits), in `vocab`'s words ([`parse`]). Called once, before fd 3.
pub fn init(
    vocab: &'static Vocab,
    dir: Option<(OwnedFd, PathBuf)>,
    levels: u64,
    salt: Option<String>,
) {
    // The words first: a record made before (or without) the drainer still has them on stderr.
    let _ = runecraft_log::set_vocab(vocab);
    let mut cfg = runecraft_log::Config::new("host", vocab);
    cfg.echo_stderr = dir.is_none();
    cfg.dir = dir;
    cfg.levels = levels;
    cfg.salt = salt;
    match runecraft_log::install(cfg) {
        Ok(i) => {
            if let Some(f) = &i.file {
                summoner_sys::crash::set_diag_file(f);
                summoner_sys::crash::set_diag_seq_source(runecraft_log::flushed_seq);
            }
            summoner_sys::proc_exit::set_pre_exit(pre_exit);
        }
        Err(e) => eprintln!("[summoner-host] diagnostic log: {e}"),
    }
}

/// Before every `_exit`: `session.end` and the last records (bounded by the drainer's join).
fn pre_exit() {
    runecraft_log::shutdown();
}

/// A chain thread's realtime ring (made on the message thread before the thread starts; empty without a drainer),
/// writing the host's `chain.block` / `chain.slow` rows (area `host`) with the chain's key.
pub struct RtLog {
    ring: Option<runecraft_log::RtLog>,
    chain: u64,
}

/// The ring for chain `chain`'s thread, named `thread`.
pub fn rt_ring(thread: &'static str, chain: u64) -> RtLog {
    RtLog {
        ring: runecraft_log::RtLog::new(thread),
        chain,
    }
}

impl RtLog {
    /// A ring nothing reads (tests).
    #[cfg(test)]
    pub fn none() -> RtLog {
        RtLog {
            ring: None,
            chain: 0,
        }
    }

    /// One processed block: `chain.block {chain, ticks, frames, seq}` (trace). Realtime-safe: a level check, then a
    /// push or a counted drop.
    #[inline]
    pub fn block(&mut self, ticks: u64, frames: u64, seq: u64) {
        if let Some(r) = &mut self.ring {
            r.event(code::CHAIN_BLOCK, 0, [self.chain, ticks, frames, seq, 0]);
        }
    }

    /// A block over its budget: `chain.slow {chain, ticks, budgetTicks, frames, seq}` (info, off by default).
    #[inline]
    pub fn slow(&mut self, ticks: u64, budget_ticks: u64, frames: u64, seq: u64) {
        if let Some(r) = &mut self.ring {
            r.event(
                code::CHAIN_SLOW,
                0,
                [self.chain, ticks, budget_ticks, frames, seq],
            );
        }
    }
}

#[cfg(test)]
mod tests;
