//! A chain region and the child page as typed views over a [`Mapping`]. Every access is an atomic at
//! the field's declared width; every offset comes from the host application's own [`Geometry`], never from the mapping. Values
//! read here are the peer's and untrusted: callers clamp them ([`crate::rt`] does).

use crate::sync::{AtomicU32, AtomicU64, Doorbell, DoorbellKind, Mapping, Ordering, Waker};
#[cfg(not(loom))]
use runecraft_ipc::channel::Transferable;
use std::io;
use summoner_proto::layout::*;

/// One chain's region: the mapping, the geometry the host application chose, and the owned doorbells of its `seq` and `ack` words
/// (the region owns them; the realtime code rings their borrowed [`Waker`]s).
#[derive(Debug)]
pub struct Region {
    map: Mapping,
    geo: Geometry,
    chain_key: u64,
    instance_id: u64,
    seq_bell: Doorbell,
    ack_bell: Doorbell,
}

/// A region's two doorbells (`seq`, `ack`: one waiter each).
fn one_bells() -> io::Result<(Doorbell, Doorbell)> {
    Ok((
        Doorbell::create(DoorbellKind::One)?,
        Doorbell::create(DoorbellKind::One)?,
    ))
}

impl Region {
    /// The loom model's region: `geo.region_bytes()` of model words (no mapping, no descriptor), the
    /// header written once as `create` writes it.
    #[cfg(loom)]
    pub fn model(geo: Geometry, chain_key: u64, instance_id: u64) -> Region {
        let map = Mapping::model(geo.region_bytes());
        let (seq_bell, ack_bell) = one_bells().expect("the model's doorbells");
        let r = Region {
            map,
            geo,
            chain_key,
            instance_id,
            seq_bell,
            ack_bell,
        };
        r.write_header(0, 48_000.0);
        r
    }

    /// The header [0, 128), written once by the host application before the region is handed over.
    fn write_header(&self, flags: u32, sample_rate: f64) {
        let (map, geo) = (&self.map, &self.geo);
        let w32 = |o: usize, v: u32| map.u32(o).store(v, Ordering::Relaxed);
        let w64 = |o: usize, v: u64| map.u64(o).store(v, Ordering::Relaxed);
        w32(H_MAGIC, MAGIC);
        w32(H_ABI, ABI_VERSION);
        w32(H_REGION_BYTES, geo.region_bytes() as u32);
        w32(H_FLAGS, flags);
        w64(H_CHAIN_KEY, self.chain_key);
        w64(H_INSTANCE_ID, self.instance_id);
        w64(H_SAMPLE_RATE, sample_rate.to_bits());
        w32(H_MAX_FRAMES, geo.max_frames);
        w32(H_CH_IN, geo.ch_in);
        w32(H_CH_OUT, geo.ch_out);
        w32(H_MAX_MIDI, geo.max_midi);
        w32(H_MAX_PARAMS, geo.max_params);
        w32(H_TAP_SLOT, geo.tap_slot);
        w32(H_SLOT_BYTES, geo.slot_bytes() as u32);
        w32(H_SLOT_OFF0, geo.slot_off(0) as u32);
        w32(H_SLOT_OFF1, geo.slot_off(1) as u32);
        w32(H_IN_OFF, geo.in_off() as u32);
        w32(H_OUT_OFF, geo.out_off() as u32);
        w32(H_MIDI_OFF, geo.midi_off() as u32);
        w32(H_PARAM_OFF, geo.param_off() as u32);
    }

    /// The supervisor's creation: an unlinked shm object of `geo.mapped_bytes()`, the header written once.
    /// Returns the region and the object `chain.load` gives the child (on unix its descriptor, by `SCM_RIGHTS`).
    #[cfg(not(loom))]
    #[allow(clippy::disallowed_methods)] // control side: the supervisor creates regions
    pub fn create(
        geo: Geometry,
        chain_key: u64,
        sample_rate: f64,
        flags: u32,
    ) -> io::Result<(Region, Transferable)> {
        geo.validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.bound()))?;
        let (map, fd) = Mapping::create(geo.mapped_bytes())?;
        let (seq_bell, ack_bell) = one_bells()?;
        let r = Region {
            map,
            geo,
            chain_key,
            instance_id: runecraft_ipc::instance::instance_id(),
            seq_bell,
            ack_bell,
        };
        r.write_header(flags, sample_rate);
        // Publish the header before the descriptor leaves this thread.
        std::sync::atomic::fence(Ordering::Release);
        Ok((r, fd))
    }

    /// Wire this process's view of the region in place (`mlock`), counted; idempotent. Each side
    /// wires its own mapping (a wired page can still be unmapped in the other process's page table). The bytes wired,
    /// or the refusal (the region stays usable, unwired: count it).
    #[cfg(not(loom))]
    pub fn wire(&self) -> Result<u64, runecraft_rt::os::LockError> {
        self.map.wire().map(|n| n as u64)
    }

    /// The bytes [`Region::wire`] wired (0: not wired).
    #[cfg(not(loom))]
    pub fn wired_bytes(&self) -> u64 {
        self.map.wired_bytes() as u64
    }

    /// The objects the child's [`Region::open`] needs beside the region itself: its doorbells' (none on unix, so
    /// `chain.load` carries exactly the region).
    #[cfg(not(loom))]
    pub fn bell_transferables(&self) -> io::Result<Vec<Transferable>> {
        let mut v = self.seq_bell.transferables()?;
        v.extend(self.ack_bell.transferables()?);
        Ok(v)
    }

    /// The child side: map a received region whose geometry `chain.load` named, and check the header against it.
    /// `bells` are the doorbells' objects that came with it ([`Region::bell_transferables`]; none on unix).
    #[cfg(not(loom))]
    pub fn open(
        region: Transferable,
        bells: &mut Vec<Transferable>,
        geo: Geometry,
        chain_key: u64,
    ) -> io::Result<Region> {
        geo.validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.bound()))?;
        let map = Mapping::from_transferable(region, geo.mapped_bytes())?;
        std::sync::atomic::fence(Ordering::Acquire);
        let r32 = |o: usize| map.u32(o).load(Ordering::Relaxed);
        let ok = r32(H_MAGIC) == MAGIC
            && r32(H_ABI) >> 16 == ABI_VERSION >> 16
            && r32(H_MAX_FRAMES) == geo.max_frames
            && r32(H_CH_IN) == geo.ch_in
            && r32(H_CH_OUT) == geo.ch_out
            && r32(H_SLOT_BYTES) as usize == geo.slot_bytes()
            && map.u64(H_CHAIN_KEY).load(Ordering::Relaxed) == chain_key;
        if !ok {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "region header does not match chain.load",
            ));
        }
        let instance_id = map.u64(H_INSTANCE_ID).load(Ordering::Relaxed);
        let seq_bell = Doorbell::from_transferables(DoorbellKind::One, bells)?;
        let ack_bell = Doorbell::from_transferables(DoorbellKind::One, bells)?;
        Ok(Region {
            map,
            geo,
            chain_key,
            instance_id,
            seq_bell,
            ack_bell,
        })
    }

    #[inline]
    /// The region's geometry.
    pub fn geometry(&self) -> &Geometry {
        &self.geo
    }

    /// The chain key the region was made for (the header's).
    pub fn chain_key(&self) -> u64 {
        self.chain_key
    }

    /// The region's instance id (the header's).
    pub fn instance_id(&self) -> u64 {
        self.instance_id
    }

    /// The header's flags word.
    pub fn flags(&self) -> u32 {
        self.map.u32(H_FLAGS).load(Ordering::Relaxed)
    }

    /// The header's sample rate.
    pub fn sample_rate(&self) -> f64 {
        f64::from_bits(self.map.u64(H_SAMPLE_RATE).load(Ordering::Relaxed))
    }

    #[inline]
    /// The doorbell's `seq` word: the last published block (its parity picks the slot).
    pub fn seq(&self) -> &AtomicU64 {
        self.map.u64(D_SEQ)
    }

    #[inline]
    /// The `seq` word's doorbell: the host application rings it for a parked or render chain (one waiter, the chain's thread).
    pub fn seq_waker(&self) -> Waker<'_> {
        self.seq_bell.waker(self.seq())
    }

    #[inline]
    /// The `ack` word's doorbell: the child rings it after a render block (one waiter, the render worker).
    pub fn ack_waker(&self) -> Waker<'_> {
        self.ack_bell.waker(self.ack())
    }

    #[inline]
    /// The doorbell's ping word: the wedge detector's nonce (0 = none).
    pub fn ping(&self) -> &AtomicU64 {
        self.map.u64(D_PING)
    }

    #[inline]
    /// The ack line's `ack` word: the `seq` of the last completed block.
    pub fn ack(&self) -> &AtomicU64 {
        self.map.u64(A_ACK)
    }

    #[inline]
    /// The ack line's cumulative count of processed blocks.
    pub fn processed(&self) -> &AtomicU64 {
        self.map.u64(A_PROCESSED)
    }

    #[inline]
    /// The ack line's pong word: the last ping the chain thread answered.
    pub fn pong(&self) -> &AtomicU64 {
        self.map.u64(A_PONG)
    }

    #[inline]
    /// The ack line's in-slot word: the slot + 1 while the child is inside a plugin's block, else 0.
    pub fn in_slot(&self) -> &AtomicU32 {
        self.map.u32(A_IN_SLOT)
    }

    #[inline]
    /// The ack line's ring-group word, written by the child's reader.
    pub fn group(&self) -> &AtomicU32 {
        self.map.u32(A_GROUP)
    }

    #[inline]
    /// The ack line's histogram: block counts by process time over the period, in 1/20 steps.
    pub fn hist(&self) -> &[AtomicU32] {
        self.map.u32s(A_HIST, HIST_BUCKETS)
    }

    /// Slot `seq & 1`.
    #[inline]
    pub fn slot(&self, seq: u64) -> Slot<'_> {
        Slot {
            map: &self.map,
            base: self.geo.slot_off((seq & 1) as usize),
            geo: &self.geo,
        }
    }
}

/// One slot's lines and planes.
#[derive(Clone, Copy)]
pub struct Slot<'a> {
    map: &'a Mapping,
    base: usize,
    geo: &'a Geometry,
}

impl<'a> Slot<'a> {
    #[inline]
    /// The 32-bit word at `off` in this slot.
    pub fn u32(&self, off: usize) -> &'a AtomicU32 {
        self.map.u32(self.base + off)
    }

    #[inline]
    /// The 64-bit word at `off` in this slot.
    pub fn u64(&self, off: usize) -> &'a AtomicU64 {
        self.map.u64(self.base + off)
    }

    /// Input channel `c` (`max_frames` samples as f32 bits).
    #[inline]
    pub fn input(&self, c: usize) -> &'a [AtomicU32] {
        let mf = self.geo.max_frames as usize;
        assert!(c < self.geo.ch_in as usize);
        self.map
            .u32s(self.base + self.geo.in_off() + c * mf * 4, mf)
    }

    /// Output channel `c`.
    #[inline]
    pub fn output(&self, c: usize) -> &'a [AtomicU32] {
        let mf = self.geo.max_frames as usize;
        assert!(c < self.geo.ch_out as usize);
        self.map
            .u32s(self.base + self.geo.out_off() + c * mf * 4, mf)
    }

    /// MIDI record `i`: `[offset, bytes, data0..data3]` as six words.
    #[inline]
    pub fn midi(&self, i: usize) -> &'a [AtomicU32] {
        assert!(i < self.geo.max_midi as usize);
        self.map
            .u32s(self.base + self.geo.midi_off() + i * MIDI_RECORD, 6)
    }

    /// Parameter change `i`: `[id, value bits]`.
    #[inline]
    pub fn param(&self, i: usize) -> &'a [AtomicU32] {
        assert!(i < self.geo.max_params as usize);
        self.map
            .u32s(self.base + self.geo.param_off() + i * PARAM_RECORD, 2)
    }
}

/// The child page: the hub words (supervisor -> child) with their owned FanOut doorbells, and the status record and
/// counters (child -> supervisor).
#[derive(Debug)]
pub struct ChildPage {
    map: Mapping,
    hubs: [Doorbell; RING_GROUPS],
}

/// The page's hub doorbells, one per ring group (every chain thread of a group waits on its hub).
fn hub_bells(
    mut make: impl FnMut() -> io::Result<Doorbell>,
) -> io::Result<[Doorbell; RING_GROUPS]> {
    Ok([make()?, make()?, make()?, make()?])
}

/// The status record as the host application reads it after a death or on its tick (length-clamped, UTF-8-lossy).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct StatusRecord {
    /// The op the child was in (`OP_*`).
    pub op: u32,
    /// The slot of that op.
    pub slot: u32,
    /// The chain key of that op.
    pub chain_key: u64,
    /// When the op started, in host ticks (0: no op in flight).
    pub op_start_ticks: u64,
    /// The plugin's name, as the child wrote it.
    pub name: String,
}

impl StatusRecord {
    /// A value of these fields. The struct is `#[non_exhaustive]`: a field added later is optional and starts
    /// at its default here.
    #[inline]
    pub fn new(
        op: u32,
        slot: u32,
        chain_key: u64,
        op_start_ticks: u64,
        name: String,
    ) -> StatusRecord {
        StatusRecord {
            op,
            slot,
            chain_key,
            op_start_ticks,
            name,
        }
    }
}

impl ChildPage {
    /// The loom model's child page: the hub lines and the status record and counters (no mapping).
    #[cfg(loom)]
    pub fn model() -> ChildPage {
        ChildPage {
            map: Mapping::model(P_WG_JOINED + 8),
            hubs: hub_bells(|| Doorbell::create(DoorbellKind::FanOut))
                .expect("the model's doorbells"),
        }
    }

    #[cfg(not(loom))]
    #[allow(clippy::disallowed_methods)] // control side
    /// The host application side: create a zero-filled child page and the object `hello` gives the child.
    pub fn create() -> io::Result<(ChildPage, Transferable)> {
        let (map, fd) = Mapping::create(CHILD_PAGE_BYTES)?;
        let hubs = hub_bells(|| Doorbell::create(DoorbellKind::FanOut))?;
        Ok((ChildPage { map, hubs }, fd))
    }

    /// The objects the child's [`ChildPage::open`] needs beside the page: its hub doorbells' (none on unix).
    #[cfg(not(loom))]
    pub fn bell_transferables(&self) -> io::Result<Vec<Transferable>> {
        let mut v = Vec::new();
        for h in &self.hubs {
            v.extend(h.transferables()?);
        }
        Ok(v)
    }

    /// Wire this process's view of the child page in place (`mlock`), counted; idempotent. The
    /// bytes wired, or the refusal (the page stays usable, unwired: count it).
    #[cfg(not(loom))]
    pub fn wire(&self) -> Result<u64, runecraft_rt::os::LockError> {
        self.map.wire().map(|n| n as u64)
    }

    /// The bytes [`ChildPage::wire`] wired (0: not wired).
    #[cfg(not(loom))]
    pub fn wired_bytes(&self) -> u64 {
        self.map.wired_bytes() as u64
    }

    #[cfg(not(loom))]
    /// The child side: map a received child page; `bells` are its hub doorbells' objects (none on unix).
    pub fn open(page: Transferable, bells: &mut Vec<Transferable>) -> io::Result<ChildPage> {
        Ok(ChildPage {
            map: Mapping::from_transferable(page, CHILD_PAGE_BYTES)?,
            hubs: hub_bells(|| Doorbell::from_transferables(DoorbellKind::FanOut, bells))?,
        })
    }

    #[inline]
    /// Ring group `group`'s hub word (clamped to the last group).
    pub fn hub(&self, group: usize) -> &AtomicU64 {
        self.map.u64(P_HUB[group.min(RING_GROUPS - 1)])
    }

    #[inline]
    /// Ring group `group`'s hub doorbell on its word (clamped to the last group): the callback's ring
    /// ([`crate::rt::ring_group`]), the supervisor's wedge ping, the child's hub switch.
    pub fn hub_waker(&self, group: usize) -> Waker<'_> {
        let g = group.min(RING_GROUPS - 1);
        self.hubs[g].waker(self.map.u64(P_HUB[g]))
    }

    #[inline]
    /// The 32-bit word at `off` in the page.
    pub fn word32(&self, off: usize) -> &AtomicU32 {
        self.map.u32(off)
    }

    #[inline]
    /// The 64-bit word at `off` in the page.
    pub fn word64(&self, off: usize) -> &AtomicU64 {
        self.map.u64(off)
    }

    /// The child writes its status record before a message-thread op (Relaxed word stores; `op_start_ticks` last).
    pub fn write_status(&self, op: u32, chain_key: u64, slot: u32, name: &str, ticks: u64) {
        let m = &self.map;
        m.u32(P_OP).store(op, Ordering::Relaxed);
        m.u32(P_SLOT).store(slot, Ordering::Relaxed);
        m.u64(P_CHAIN_KEY).store(chain_key, Ordering::Relaxed);
        let mut buf = [0u8; P_NAME_BYTES];
        let b = name.as_bytes();
        let n = b.len().min(P_NAME_BYTES);
        buf[..n].copy_from_slice(&b[..n]);
        for (i, w) in buf.as_chunks::<4>().0.iter().enumerate() {
            m.u32(P_NAME + 4 * i)
                .store(u32::from_le_bytes(*w), Ordering::Relaxed);
        }
        m.u64(P_OP_START_TICKS).store(ticks, Ordering::Release);
    }

    /// The child clears its status record after the op (`op_start_ticks` first).
    pub fn clear_status(&self) {
        self.map.u64(P_OP_START_TICKS).store(0, Ordering::Release);
        self.map.u32(P_OP).store(OP_NONE, Ordering::Relaxed);
    }

    /// The host application's read of the status record.
    pub fn read_status(&self) -> StatusRecord {
        let m = &self.map;
        let ticks = m.u64(P_OP_START_TICKS).load(Ordering::Acquire);
        let mut bytes = [0u8; P_NAME_BYTES];
        for i in 0..P_NAME_BYTES / 4 {
            bytes[4 * i..4 * i + 4]
                .copy_from_slice(&m.u32(P_NAME + 4 * i).load(Ordering::Relaxed).to_le_bytes());
        }
        let n = bytes.iter().position(|&b| b == 0).unwrap_or(P_NAME_BYTES);
        StatusRecord {
            op: m.u32(P_OP).load(Ordering::Relaxed),
            slot: m.u32(P_SLOT).load(Ordering::Relaxed),
            chain_key: m.u64(P_CHAIN_KEY).load(Ordering::Relaxed),
            op_start_ticks: ticks,
            name: String::from_utf8_lossy(&bytes[..n]).into_owned(),
        }
    }
}
