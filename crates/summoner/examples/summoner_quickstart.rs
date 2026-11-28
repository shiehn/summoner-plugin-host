//! Start the supervisor, load one chain and watch its events. The supervisor spawns the host program (the helper
//! process that loads the plugins), does the handshake, and reports what happens as events on a channel you own.
//!
//! ```sh
//! cargo build -p summoner-host
//! cargo run -p summoner --example summoner_quickstart -- target/debug/summoner-host [<bundle.vst3> <class id>]
//! ```
//!
//! With no plugin named, the chain holds one empty slot, which passes its input through.
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

use runecraft_contracts::digest::NoStateHash;
use summoner::supervisor::{Bridge, ChainSpec, Command, Config, Event};
use summoner_proto::codec::Role;
use summoner_proto::layout::Geometry;
use summoner_proto::plugin::PluginRef;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let Some(program) = args.next() else {
        eprintln!("usage: summoner_quickstart <host program> [<bundle.vst3> <class id>]");
        return Ok(());
    };
    let plugin = match (args.next(), args.next()) {
        (Some(bundle), Some(class_id)) => Some(PluginRef::vst3(bundle, class_id, "plugin")?),
        _ => None,
    };

    // The state hasher is your application's choice; NoStateHash reports no digest.
    let config = Config::new(program.into(), 48_000.0, Arc::new(NoStateHash));
    let (events_tx, events) = sync_channel(config.event_bound);
    let bridge = Bridge::start(config, events_tx)?;

    // One live stereo chain of one slot, for blocks of up to 512 frames. Sending never waits: a full command queue
    // hands the command back (`Busy`) for you to retry.
    let spec = ChainSpec::new(Geometry::stereo(512), Role::Live, vec![plugin]);
    if let Err(busy) = bridge.send(Command::Load {
        key: 1,
        spec,
        states: vec![],
    }) {
        eprintln!("the command queue is full: {:?}", busy.0);
    }

    // React on your control thread. `ChainReady` carries the link your audio callback exchanges blocks through.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(Event::ChildUp {
                pid, incarnation, ..
            }) => println!("host program up: pid {pid}, incarnation {incarnation}"),
            Ok(Event::ChainReady {
                key, slots, link, ..
            }) => {
                println!(
                    "chain {key} ready: {} slot(s), delay compensation {} frames",
                    slots.len(),
                    link.pdc_latency(512)
                );
                break;
            }
            Ok(Event::ChainDown { key, reason }) => {
                println!("chain {key} down: {reason:?}");
                break;
            }
            Ok(other) => println!("event: {other:?}"),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    bridge.shutdown();
    Ok(())
}
