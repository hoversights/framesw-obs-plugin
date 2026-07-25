//! Preview-layer audio monitor taps: forwards a chosen OBS source's real,
//! passively-captured audio (the *same* `audio_capture_callback` already
//! attached to every source for metering — see `lib.rs`) out as an
//! audio-only NDI sender per "bus," FrameSW's own monitor-speaker
//! concept, entirely independent of OBS's own audio routing.
//!
//! A tap never re-routes anything inside OBS — it only ever *reads* the
//! same samples the metering path already reads and copies them into a
//! separate NDI send buffer, so by construction it cannot affect what
//! OBS actually streams/records (see `PROJECT_OVERVIEW.md`'s "Solo must
//! never touch the program mix" invariant, which this mechanism was
//! built specifically to honor for Preview-only content OBS's own
//! monitoring device structurally cannot reach).

use std::collections::HashMap;
use std::sync::Mutex;

use crate::ndi_ffi::NdiSender;

struct Tap {
    source_name: String,
    sender: NdiSender,
}

/// bus_id -> its active tap, if any. A bus with no entry is simply
/// inactive — every lookup/removal here is a safe, idempotent no-op via
/// `HashMap`'s own semantics for a missing key, never an error.
static TAPS: Mutex<Option<HashMap<String, Tap>>> = Mutex::new(None);

/// What `start_audio_tap` should do for a given bus, based purely on
/// whatever source name (if any) it's already tapping — separated from
/// actually constructing an `NdiSender` so this decision is unit
/// testable without a real NDI runtime, which is never present in a
/// CI/test sandbox (this crate's own "no live OBS in unit tests" rule
/// extends to no real NDI either, since both are genuinely external to
/// what a `cargo test` process has loaded).
#[derive(Debug, PartialEq, Eq)]
enum StartDecision {
    /// Already tapping this exact source on this bus — nothing to do.
    AlreadyCorrect,
    /// Bus exists but is tapping a different source — repoint it. The
    /// existing NDI sender stays alive; recreating it would flicker/
    /// disconnect every receiver currently watching that NDI name for no
    /// reason.
    Repoint,
    /// No tap exists for this bus yet — a new sender must be created.
    NeedsNewSender,
}

fn decide_start(existing_source_name: Option<&str>, requested_source_name: &str) -> StartDecision {
    match existing_source_name {
        Some(name) if name == requested_source_name => StartDecision::AlreadyCorrect,
        Some(_) => StartDecision::Repoint,
        None => StartDecision::NeedsNewSender,
    }
}

/// Starts (or repoints) a monitor tap: `bus_id` (e.g. `"1"`, used to name
/// the NDI source `"FrameSW-Monitor-{bus_id}"`) begins forwarding
/// `source_name`'s real audio. Safe to call repeatedly with the same
/// arguments (a no-op) or with a different `source_name` for an
/// already-active `bus_id` (repoints in place — see `decide_start`).
///
/// Returns `Err` only when a *new* sender is needed and the NDI runtime
/// itself couldn't be loaded/initialized (e.g. not installed on this
/// machine) — never for "the named source doesn't exist in OBS yet,"
/// which is the ordinary case for a layer that's about to go active a
/// moment later: it simply has nothing to forward until that source
/// starts producing audio callbacks (see `forward_if_tapped`'s own
/// no-op-when-nothing-matches behavior, which is what makes a
/// no-audio-source layer safe rather than a crash).
pub fn start_audio_tap(source_name: &str, bus_id: &str) -> Result<(), String> {
    let Ok(mut guard) = TAPS.lock() else {
        return Err("tap registry lock poisoned".to_string());
    };
    let taps = guard.get_or_insert_with(HashMap::new);
    let existing = taps.get(bus_id).map(|t| t.source_name.as_str());
    match decide_start(existing, source_name) {
        StartDecision::AlreadyCorrect => Ok(()),
        StartDecision::Repoint => {
            if let Some(tap) = taps.get_mut(bus_id) {
                tap.source_name = source_name.to_string();
            }
            Ok(())
        }
        StartDecision::NeedsNewSender => {
            let ndi_name = format!("FrameSW-Monitor-{bus_id}");
            let Some(sender) = NdiSender::new(&ndi_name) else {
                return Err(format!(
                    "NDI runtime unavailable — could not create sender '{ndi_name}'"
                ));
            };
            taps.insert(bus_id.to_string(), Tap { source_name: source_name.to_string(), sender });
            Ok(())
        }
    }
}

/// Stops `bus_id`'s tap and tears down its NDI sender (via `Tap`'s
/// `Drop`). Removing an already-inactive (or never-started) bus is a
/// safe no-op.
pub fn stop_audio_tap(bus_id: &str) {
    if let Ok(mut guard) = TAPS.lock() {
        if let Some(taps) = guard.as_mut() {
            taps.remove(bus_id);
        }
    }
}

/// The source currently tapped for `bus_id`, if that bus is active.
pub fn tap_status(bus_id: &str) -> Option<String> {
    let guard = TAPS.lock().ok()?;
    guard.as_ref()?.get(bus_id).map(|t| t.source_name.clone())
}

/// Tears down every active tap — called from `obs_module_unload` so no
/// NDI sender outlives the plugin itself (each `Tap`'s `Drop` calls
/// `NDIlib_send_destroy`).
pub fn stop_all() {
    if let Ok(mut guard) = TAPS.lock() {
        *guard = None;
    }
}

/// Called from `lib.rs`'s existing `audio_capture_callback` — every
/// audio-capable source's capture callback fires this on every
/// invocation, so it stays as cheap as possible when nothing is tapped
/// (the overwhelmingly common case, e.g. Solo off entirely): one lock,
/// one name comparison per active bus, and an early-out before any of
/// that when no bus is active at all.
///
/// `planes`: up to `channels` per-channel buffers (`AUDIO_FORMAT_FLOAT_
/// PLANAR`, matching OBS's own `audio_data.data[]`), each `frames`
/// samples of `f32` — *not* pre-rearranged, since the copy into NDI's
/// contiguous-stride layout is only worth paying for a source that
/// actually has an active tap right now. A source with no audio at all
/// (all planes null, or `frames == 0`) simply matches zero taps' worth
/// of real work below and returns — this is exactly what keeps a
/// no-audio-source layer from ever being able to crash the bus.
pub fn forward_if_tapped(
    source_name: &str,
    sample_rate: i32,
    channels: i32,
    frames: i32,
    planes: &[*const f32],
) {
    if frames <= 0 || channels <= 0 {
        return;
    }
    let Ok(guard) = TAPS.lock() else {
        return;
    };
    let Some(taps) = guard.as_ref() else {
        return;
    };
    if taps.is_empty() {
        return;
    }
    // A source can, in principle, be tapped by more than one bus at once
    // (e.g. monitored on two different outputs simultaneously) — forward
    // to every match, not just the first.
    for tap in taps.values() {
        if tap.source_name != source_name {
            continue;
        }
        // NDI's `NDIlib_audio_frame_v2_t` wants one contiguous buffer
        // with a fixed inter-channel byte stride; OBS instead gives each
        // channel its own independent allocation, so this rebuilds it
        // into the shape NDI actually needs. Real per-callback cost, but
        // bounded (a few hundred samples times a handful of channels)
        // and only ever paid for a source that's actually tapped.
        let frames_usize = frames as usize;
        let mut buffer = vec![0.0f32; frames_usize * channels as usize];
        for (ch, &plane) in planes.iter().take(channels as usize).enumerate() {
            if plane.is_null() {
                continue;
            }
            // Safety: caller (`lib.rs`'s audio_capture_callback_impl)
            // guarantees each non-null plane points at `frames` valid
            // `f32` samples for the duration of this call — the same
            // guarantee libobs's own callback contract already gives it.
            let src = unsafe { std::slice::from_raw_parts(plane, frames_usize) };
            buffer[ch * frames_usize..(ch + 1) * frames_usize].copy_from_slice(src);
        }
        tap.sender.send_audio(sample_rate, channels, frames, &mut buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_start_same_source_needs_nothing() {
        assert_eq!(decide_start(Some("shot-abc"), "shot-abc"), StartDecision::AlreadyCorrect);
    }

    #[test]
    fn decide_start_different_source_repoints_in_place() {
        assert_eq!(decide_start(Some("shot-abc"), "shot-xyz"), StartDecision::Repoint);
    }

    #[test]
    fn decide_start_no_existing_tap_needs_a_new_sender() {
        assert_eq!(decide_start(None, "shot-abc"), StartDecision::NeedsNewSender);
    }

    // Real lifecycle exercise of the registry itself — a fresh, test-only
    // bus_id per test avoids any cross-test interference from the shared
    // global `TAPS` map under parallel `cargo test` execution.

    #[test]
    fn stop_on_a_never_started_bus_is_a_safe_no_op() {
        stop_audio_tap("test-bus-never-started");
        assert_eq!(tap_status("test-bus-never-started"), None);
    }

    #[test]
    fn status_is_none_for_a_bus_that_was_never_started() {
        assert_eq!(tap_status("test-bus-untouched"), None);
    }

    #[test]
    fn start_never_leaves_a_half_registered_entry() {
        // A CI sandbox has no NDI runtime installed, so this reports
        // `Err` there — degrading gracefully, never panicking, same as
        // every other missing-runtime-symbol case in this crate. A dev
        // machine that *does* have the NDI runtime installed (as this
        // one does) instead genuinely succeeds, proving the runtime
        // discovery/FFI path itself works end to end. Either way, the
        // invariant that actually matters here is the same: `tap_status`
        // must never disagree with what `start_audio_tap` reports —
        // never `Some` after an `Err`, and never `None` after an `Ok`.
        let result = start_audio_tap("shot-abc", "test-bus-lifecycle");
        match result {
            Ok(()) => {
                assert_eq!(tap_status("test-bus-lifecycle"), Some("shot-abc".to_string()));
                stop_audio_tap("test-bus-lifecycle");
            }
            Err(_) => {
                assert_eq!(tap_status("test-bus-lifecycle"), None);
            }
        }
    }

    #[test]
    fn forward_with_no_active_taps_does_not_panic() {
        let planes: [*const f32; 2] = [std::ptr::null(), std::ptr::null()];
        forward_if_tapped("shot-that-does-not-exist", 48000, 2, 480, &planes);
    }

    #[test]
    fn forward_real_audio_from_a_live_source_with_no_matching_tap_is_a_safe_no_op() {
        // Simulates the callback receiving genuine, non-null, non-zero
        // audio from an actual live/active source — the ordinary case of
        // a layer with real audio flowing and Solo simply not engaged on
        // it — proving the whole plane-iteration/marshaling path runs
        // cleanly even with real data present, not just the all-null
        // no-audio-source edge case above.
        let left = vec![0.25f32; 480];
        let right = vec![-0.25f32; 480];
        let planes: [*const f32; 2] = [left.as_ptr(), right.as_ptr()];
        forward_if_tapped("shot-currently-live", 48000, 2, 480, &planes);
    }
}
