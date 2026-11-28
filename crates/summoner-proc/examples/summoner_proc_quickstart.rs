//! A slot of your own: a one-parameter gain as a `Plugin` (the shared, message-thread side) and the `Dsp` its
//! `prepare` makes (owned by the chain thread), then one stereo block processed in place, as the host program runs
//! every slot. Run it with `cargo run -p summoner-proc --example summoner_proc_quickstart`.
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use summoner_proc::{Block, Dsp, Error, Io, Plugin, Transport, with_channels};

/// The plugin: its one parameter lives in an atomic, because `set_param` is called on the chain thread.
struct Gain {
    gain: Arc<AtomicU32>,
}

/// The prepared processor: it reads the same atomic at each block.
struct GainDsp {
    gain: Arc<AtomicU32>,
}

impl Plugin for Gain {
    fn name(&self) -> &str {
        "Example Gain"
    }
    fn latency(&self) -> u32 {
        0
    }
    fn param_count(&self) -> u32 {
        1
    }
    fn param_name(&self, _index: u32) -> String {
        "Gain".into()
    }
    fn set_param(&self, index: u32, value: f32) {
        if index == 0 {
            self.gain.store(value.to_bits(), Ordering::Relaxed);
        }
    }
    fn set_state(&self, blob: &[u8]) -> Result<(), Error> {
        let bytes: [u8; 4] = blob
            .try_into()
            .map_err(|_| Error::from("a gain state is 4 bytes"))?;
        self.gain
            .store(u32::from_le_bytes(bytes), Ordering::Relaxed);
        Ok(())
    }
    fn get_state(&self) -> Vec<u8> {
        self.gain.load(Ordering::Relaxed).to_le_bytes().to_vec()
    }
    fn prepare(&self, _rate: f64, _max_frames: usize) -> Box<dyn Dsp> {
        Box::new(GainDsp {
            gain: self.gain.clone(),
        })
    }
}

impl Dsp for GainDsp {
    fn io(&self) -> Io {
        Io::STEREO
    }
    fn reset(&mut self) {}
    fn process(&mut self, block: &mut Block<'_>) -> bool {
        // The block's channels are realtime buffers: no allocation, no lock, no system call in here.
        let g = f32::from_bits(self.gain.load(Ordering::Relaxed));
        for ch in block.channels.iter_mut() {
            for s in &mut ch[..block.frames] {
                *s *= g;
            }
        }
        true
    }
}

fn main() {
    let plugin = Gain {
        gain: Arc::new(AtomicU32::new(1.0f32.to_bits())),
    };
    plugin.set_param(0, 0.5);
    let mut dsp = plugin.prepare(48_000.0, 64);
    let mut channels = vec![vec![1.0f32; 64]; 2];
    let transport = Transport::default();
    let ok = with_channels(&mut channels, |channels| {
        dsp.process(&mut Block {
            frames: 64,
            channels,
            midi: &[],
            transport: &transport,
        })
    });
    assert!(ok && channels.iter().flatten().all(|s| *s == 0.5));
    // The state round trip a host uses to save and restore the slot.
    let saved = plugin.get_state();
    plugin.set_param(0, 1.0);
    plugin.set_state(&saved).expect("its own state");
    assert_eq!(plugin.get_state(), saved);
    println!("{}: one block at gain 0.5, state {saved:?}", plugin.name());
}

#[test]
fn the_example_runs() {
    main();
}
