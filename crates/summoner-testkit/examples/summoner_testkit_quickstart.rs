//! The test double as a program of your own: a host program that speaks the real protocol over trivial DSP, for
//! tests that must exercise the supervisor's every path (a crash, a hang, a slow block) without a third-party plugin.
//! The supervisor spawns it like the real host program.
//!
//! ```sh
//! cargo build -p summoner-testkit --example summoner_testkit_quickstart
//! cargo run -p summoner --example summoner_quickstart -- target/debug/examples/summoner_testkit_quickstart
//! ```
fn main() {
    summoner_testkit::fake_host::main();
}
