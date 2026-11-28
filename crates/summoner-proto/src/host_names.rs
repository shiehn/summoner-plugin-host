//! The names a host and its parent must agree on outside the frames: the crash log's file name the host writes into
//! the folder its parent passes (`--crash-log <dir>`), and the test-hook variable that turns a lab build's death
//! knobs on. One definition, so the two processes (and the test double) cannot drift.

/// The stem of a host's crash log: `<dir>/<CRASH_LOG_STEM>-<pid>.log`.
pub const CRASH_LOG_STEM: &str = "summoner-host-crash";

/// The crash log's file name for the host process `pid` (control side: it allocates).
#[allow(clippy::disallowed_methods)]
pub fn crash_log_name(pid: u32) -> String {
    format!("{CRASH_LOG_STEM}-{pid}.log")
}

/// The first line of a host's crash report (after a leading newline), as the host writes it.
pub const CRASH_BANNER: &str = "==== summoner-host crash ====";

/// The lab's death switch: a test build's death knobs (a crash on load, a crash parameter, a half frame) act only when
/// this variable is exactly `kill`, and then end the process with a SIGKILL on itself (no crash report). A release
/// build never reads it.
pub const LAB_DEATH_ENV: &str = "SUMMONER_LAB_DEATH";

#[cfg(test)]
mod tests {
    #[test]
    fn the_crash_log_is_named_by_the_pid() {
        assert_eq!(super::crash_log_name(42), "summoner-host-crash-42.log");
    }
}
