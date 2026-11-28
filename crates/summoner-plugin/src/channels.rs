//! The slot channel rule.
//!
//! A hosted chain runs on a block of exactly two channels (channel 0 = left, channel 1 = right). Each plugin slot
//! declares, once per prepare, how many input channels it takes (`inputs`) and how many output channels it writes
//! (`outputs`). Around each call of a slot's plugin the host adapts the two-channel block to the slot's bus shape.
//! The plugin host and the fake host both use this one implementation. Only frames `0..frames` are touched. Samples
//! past `frames` stay exactly as they were, in every channel.
//!
//! Before the call (what the plugin sees):
//! 1. Average: when `inputs == 1` and `outputs != 2`, channel 0 becomes `(L + R) * 0.5`. That is one `f32`
//!    addition followed by one `f32` multiplication by 0.5: no fused multiply-add, no reassociation, no wider
//!    intermediate. With `inputs == 1` and `outputs == 2`, channel 0 is left as it is.
//! 2. Zero the unused inputs: every channel whose index is `>= inputs` is set to `+0.0`.
//!
//! The average reads channel 1 before step 2 zeroes it.
//!
//! After the call (what the next slot gets):
//! 1. Zero the unused outputs: every channel whose index is `>= outputs` is set to `+0.0`.
//! 2. Mono to both: when `inputs <= 1` and `outputs <= 1`, channel 1 becomes a bit-for-bit copy of channel 0.
//!
//! Other widths: with fewer than two channels the average and the copy do nothing, and the zeroing steps apply to
//! the channels that exist. With more than two channels the average and the copy use channels 0 and 1 only, and the
//! zeroing steps apply to every channel at an index `>= inputs` / `>= outputs`.
//!
//! NaN, infinities and signed zeros pass through the IEEE operations unchanged in kind. These functions never read
//! or change the floating-point mode. They do not allocate, lock or make syscalls. The only way they can panic is a
//! caller bug: `frames` larger than a channel's length.

#![forbid(unsafe_code)]

use core::ops::DerefMut;

/// Converts a declared channel count to an index bound that saturates instead of truncating.
#[inline]
fn bound(count: u32) -> usize {
    usize::try_from(count).unwrap_or(usize::MAX)
}

/// Sets `bufs[c][..frames]` to `+0.0` for every `c >= first`.
#[inline]
fn zero_from<B: DerefMut<Target = [f32]>>(bufs: &mut [B], first: usize, frames: usize) {
    if first >= bufs.len() {
        return;
    }
    for buf in bufs[first..].iter_mut() {
        buf[..frames].fill(0.0);
    }
}

/// Before the slot's plugin is called. `bufs`: the block's channels (2 in every caller). Only `bufs[c][..frames]`
/// is read or written.
#[inline]
pub fn channel_rule_before<B: DerefMut<Target = [f32]>>(
    inputs: u32,
    outputs: u32,
    bufs: &mut [B],
    frames: usize,
) {
    // Step 1: average channels 0 and 1 into channel 0 (reads channel 1 before step 2 zeroes it).
    if inputs == 1 && outputs != 2 && bufs.len() >= 2 {
        let (head, tail) = bufs.split_at_mut(1);
        let left = &mut head[0][..frames];
        let right = &tail[0][..frames];
        for (l, r) in left.iter_mut().zip(right.iter()) {
            let sum: f32 = *l + *r;
            *l = sum * 0.5_f32;
        }
    }
    // Step 2: zero the unused inputs.
    zero_from(bufs, bound(inputs), frames);
}

/// After the slot's plugin returned (whatever it returned).
#[inline]
pub fn channel_rule_after<B: DerefMut<Target = [f32]>>(
    inputs: u32,
    outputs: u32,
    bufs: &mut [B],
    frames: usize,
) {
    // Step 1: zero the unused outputs.
    zero_from(bufs, bound(outputs), frames);
    // Step 2: mono to both.
    if inputs <= 1 && outputs <= 1 && bufs.len() >= 2 {
        let (head, tail) = bufs.split_at_mut(1);
        tail[0][..frames].copy_from_slice(&head[0][..frames]);
    }
}
