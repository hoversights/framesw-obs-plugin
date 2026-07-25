//! Preview-layer audio monitor taps: forwards chosen OBS sources' real,
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
//!
//! Two independent mechanisms share this module, both keyed by `bus_id`:
//! - **`Tap`** (`start_audio_tap`/`stop_audio_tap`/`tap_status`): one
//!   exclusive source, forwarded directly on every matching callback —
//!   backs Solo. Unchanged since it first shipped and proven live.
//! - **`MixBus`** (`set_mix_sources`/`stop_mix_bus`): a *set* of sources,
//!   each callback overwriting only its own latest-chunk slot, summed
//!   and flushed by a dedicated timer thread — backs "no layer soloed,
//!   monitor the whole Preview mix." Needed because a scene's own
//!   composited audio does **not** reliably fire through this callback
//!   while it's merely Studio Mode's Preview (confirmed live,
//!   2026-07-24) — unlike individual sources, which do (Phase 1's
//!   founding discovery) — so the combined mix has to be built here,
//!   from the same individual per-source taps that already work,
//!   instead of tapping the scene directly.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ndi_ffi::NdiSender;

struct Tap {
    source_name: String,
    sender: NdiSender,
}

/// bus_id -> its active exclusive tap, if any. A bus with no entry is
/// simply inactive — every lookup/removal here is a safe, idempotent
/// no-op via `HashMap`'s own semantics for a missing key, never an
/// error.
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

/// Starts (or repoints) an exclusive monitor tap: `bus_id` (e.g.
/// `"preview"`, used to name the NDI source `"FrameSW-Monitor-{bus_id}"`)
/// begins forwarding `source_name`'s real audio, alone. Safe to call
/// repeatedly with the same arguments (a no-op) or with a different
/// `source_name` for an already-active `bus_id` (repoints in place —
/// see `decide_start`). Also tears down any `MixBus` on the same
/// `bus_id` — the two mechanisms are mutually exclusive per bus (Solo
/// engaged means the mix is not currently what's being monitored).
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
    stop_mix_bus(bus_id);
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

/// Stops `bus_id`'s exclusive tap and tears down its NDI sender (via
/// `Tap`'s `Drop`). Removing an already-inactive (or never-started) bus
/// is a safe no-op.
pub fn stop_audio_tap(bus_id: &str) {
    if let Ok(mut guard) = TAPS.lock() {
        if let Some(taps) = guard.as_mut() {
            taps.remove(bus_id);
        }
    }
}

/// The source currently tapped for `bus_id`, if that bus has an active
/// exclusive tap (not a mix bus — see `mix_bus_sources` for that).
pub fn tap_status(bus_id: &str) -> Option<String> {
    let guard = TAPS.lock().ok()?;
    guard.as_ref()?.get(bus_id).map(|t| t.source_name.clone())
}

/// Tears down every active exclusive tap and mix bus — called from
/// `obs_module_unload` so no NDI sender outlives the plugin itself.
pub fn stop_all() {
    if let Ok(mut guard) = TAPS.lock() {
        *guard = None;
    }
    if let Ok(mut guard) = MIX_BUSES.lock() {
        if let Some(buses) = guard.take() {
            for bus in buses.into_values() {
                bus.running.store(false, Ordering::Release);
            }
        }
    }
}

// ---------------------------------------------------------------------
// Mix bus: combines several sources' real audio into one NDI sender —
// backs "no layer soloed, monitor the whole Preview mix."
// ---------------------------------------------------------------------

/// How often the mix flush thread sums and sends a combined frame.
/// Matches the ballpark of a typical OBS audio callback cadence (roughly
/// one ~480-sample chunk per ~10ms at 48kHz) closely enough that a
/// contributing source's *latest* buffer is rarely more than one flush
/// cycle stale — good enough for local monitoring, not a broadcast-grade
/// synchronized mixer.
const MIX_FLUSH_INTERVAL: Duration = Duration::from_millis(10);

struct MixBus {
    sender: NdiSender,
    /// source_name -> that source's most recent post-volume planar
    /// buffer (already scaled and rearranged into contiguous per-channel
    /// runs — same shape `build_scaled_planar_buffer` produces),
    /// overwritten — never accumulated — on every matching callback.
    /// Independent sources fire at independent, uncoordinated times, so
    /// summing has to happen on its own fixed cadence (the flush
    /// thread), not per-callback.
    latest: Mutex<HashMap<String, Vec<f32>>>,
    /// Which sources currently belong to this bus's mix — read by
    /// `forward_if_tapped` to decide whether to update `latest`,
    /// written by `set_mix_sources`.
    sources: Mutex<HashSet<String>>,
    /// Most recently observed `(sample_rate, channels)` — shared by
    /// every contributor in practice, since all of OBS's audio callbacks
    /// come from the one global audio pipeline (`obs_get_audio_info`),
    /// but tracked per-bus rather than assumed so the flush thread always
    /// sends with whatever's actually current.
    meta: Mutex<(i32, i32)>,
    running: Arc<AtomicBool>,
}

/// bus_id -> its active mix bus, if any.
static MIX_BUSES: Mutex<Option<HashMap<String, Arc<MixBus>>>> = Mutex::new(None);

/// Replaces which sources contribute to `bus_id`'s combined mix. Safe to
/// call repeatedly, including with the same set (a no-op beyond updating
/// bookkeeping) or a changed set (sources no longer listed stop
/// contributing from this call on — their last-known buffer is dropped
/// immediately, not left to keep being summed as stale phantom audio).
/// Creates the bus (and its NDI sender + flush thread) on first use for
/// a given `bus_id`. Also tears down any exclusive `Tap` on the same
/// `bus_id` — mutually exclusive per bus, same as `start_audio_tap`'s
/// side of that relationship.
///
/// Returns `Err` only when a *new* bus is needed and the NDI runtime
/// couldn't be loaded — same conditions as `start_audio_tap`.
pub fn set_mix_sources(bus_id: &str, source_names: &[String]) -> Result<(), String> {
    stop_audio_tap(bus_id);
    let Ok(mut guard) = MIX_BUSES.lock() else {
        return Err("mix bus registry lock poisoned".to_string());
    };
    let buses = guard.get_or_insert_with(HashMap::new);
    let bus = match buses.get(bus_id) {
        Some(existing) => Arc::clone(existing),
        None => {
            let ndi_name = format!("FrameSW-Monitor-{bus_id}");
            let Some(sender) = NdiSender::new(&ndi_name) else {
                return Err(format!(
                    "NDI runtime unavailable — could not create sender '{ndi_name}'"
                ));
            };
            let bus = Arc::new(MixBus {
                sender,
                latest: Mutex::new(HashMap::new()),
                sources: Mutex::new(HashSet::new()),
                meta: Mutex::new((0, 0)),
                running: Arc::new(AtomicBool::new(true)),
            });
            spawn_mix_flush_thread(Arc::clone(&bus));
            buses.insert(bus_id.to_string(), Arc::clone(&bus));
            bus
        }
    };
    let new_set: HashSet<String> = source_names.iter().cloned().collect();
    if let Ok(mut sources) = bus.sources.lock() {
        *sources = new_set.clone();
    }
    if let Ok(mut latest) = bus.latest.lock() {
        latest.retain(|name, _| new_set.contains(name));
    }
    Ok(())
}

/// Tears down `bus_id`'s mix bus (stops its flush thread, destroys its
/// NDI sender). Stopping an already-inactive bus is a safe no-op.
pub fn stop_mix_bus(bus_id: &str) {
    if let Ok(mut guard) = MIX_BUSES.lock() {
        if let Some(buses) = guard.as_mut() {
            if let Some(bus) = buses.remove(bus_id) {
                bus.running.store(false, Ordering::Release);
            }
        }
    }
}

/// The set of sources currently contributing to `bus_id`'s mix, if that
/// bus is active (empty set if active but nothing's contributing yet —
/// distinct from `None`, "no mix bus exists for this bus_id at all").
pub fn mix_bus_sources(bus_id: &str) -> Option<HashSet<String>> {
    let guard = MIX_BUSES.lock().ok()?;
    let bus = guard.as_ref()?.get(bus_id)?;
    bus.sources.lock().ok().map(|s| s.clone())
}

fn spawn_mix_flush_thread(bus: Arc<MixBus>) {
    std::thread::spawn(move || {
        while bus.running.load(Ordering::Acquire) {
            std::thread::sleep(MIX_FLUSH_INTERVAL);
            if !bus.running.load(Ordering::Acquire) {
                return;
            }
            // Drained, not just read — a source that's stopped
            // delivering callbacks (unstaged, gone silent-and-inactive,
            // removed from the mix) must drop out of the *next* flush
            // rather than keep contributing its last-known buffer
            // forever.
            let contributions: Vec<Vec<f32>> = {
                let Ok(mut latest) = bus.latest.lock() else {
                    continue;
                };
                latest.drain().map(|(_, buf)| buf).collect()
            };
            let Some(mixed) = sum_contributions(&contributions) else {
                continue;
            };
            let (sample_rate, channels) = bus.meta.lock().map(|m| *m).unwrap_or((0, 0));
            if sample_rate <= 0 || channels <= 0 {
                continue;
            }
            let frames = mixed.len() as i32 / channels;
            let mut mixed = mixed;
            bus.sender.send_audio(sample_rate, channels, frames, &mut mixed);
        }
    });
}

/// Sums multiple contributors' most-recent buffers (each already
/// rearranged into contiguous per-channel runs) into one combined
/// buffer, truncating to the shortest contributor's length — a source
/// delivering a differently-sized chunk at this exact flush moment loses
/// only its tail past that point. Acceptable for a monitoring feature,
/// not broadcast-grade sample-accurate alignment. `None` if there's
/// nothing to mix (no contributors this cycle, or the shortest is
/// empty).
fn sum_contributions(contributions: &[Vec<f32>]) -> Option<Vec<f32>> {
    let frames = contributions.iter().map(Vec::len).min()?;
    if frames == 0 {
        return None;
    }
    let mut mixed = vec![0.0f32; frames];
    for buf in contributions {
        for (m, &s) in mixed.iter_mut().zip(buf.iter()) {
            *m += s;
        }
    }
    Some(mixed)
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
///
/// `volume`: the source's *current* `obs_source_get_volume` (same value
/// `lib.rs`'s metering path already reads and applies to its peak-dB
/// calculation) — applied here to the actual samples for the same
/// reason: `obs_source_add_audio_capture_callback` hands over audio
/// *before* the fader is applied (libobs applies volume later, at mix
/// time), so without this the monitor would play the raw input level
/// regardless of where the operator's own fader is sitting. Live-
/// reported: an earlier version forwarded the raw samples unscaled,
/// which correctly muted/soloed the right *source* but ignored its
/// fader position entirely.
pub fn forward_if_tapped(
    source_name: &str,
    sample_rate: i32,
    channels: i32,
    frames: i32,
    planes: &[*const f32],
    volume: f32,
) {
    if frames <= 0 || channels <= 0 {
        return;
    }
    if let Ok(guard) = TAPS.lock() {
        if let Some(taps) = guard.as_ref() {
            // A source can, in principle, be tapped by more than one bus
            // at once (e.g. monitored on two different outputs
            // simultaneously) — forward to every match, not just the
            // first.
            for tap in taps.values() {
                if tap.source_name != source_name {
                    continue;
                }
                let mut buffer = build_scaled_planar_buffer(
                    channels as usize,
                    frames as usize,
                    planes,
                    volume,
                );
                tap.sender.send_audio(sample_rate, channels, frames, &mut buffer);
            }
        }
    }
    if let Ok(guard) = MIX_BUSES.lock() {
        if let Some(buses) = guard.as_ref() {
            for bus in buses.values() {
                let belongs = bus.sources.lock().map(|s| s.contains(source_name)).unwrap_or(false);
                if !belongs {
                    continue;
                }
                let buffer =
                    build_scaled_planar_buffer(channels as usize, frames as usize, planes, volume);
                if let Ok(mut latest) = bus.latest.lock() {
                    latest.insert(source_name.to_string(), buffer);
                }
                if let Ok(mut meta) = bus.meta.lock() {
                    *meta = (sample_rate, channels);
                }
            }
        }
    }
}

/// Rebuilds OBS's independent per-channel plane buffers into the single
/// contiguous, evenly-strided `f32` buffer NDI's send API expects
/// (`channel_stride_in_bytes` in `ndi_ffi.rs`), scaling every sample by
/// `volume` along the way — split out from `forward_if_tapped` so this
/// arithmetic (the actual fix for the "plays raw input, not post-fader"
/// bug) is unit-testable without a real NDI sender or tap registry.
fn build_scaled_planar_buffer(
    channels: usize,
    frames: usize,
    planes: &[*const f32],
    volume: f32,
) -> Vec<f32> {
    let mut buffer = vec![0.0f32; frames * channels];
    for (ch, &plane) in planes.iter().take(channels).enumerate() {
        if plane.is_null() {
            continue;
        }
        // Safety: caller (`lib.rs`'s audio_capture_callback_impl)
        // guarantees each non-null plane points at `frames` valid `f32`
        // samples for the duration of this call — the same guarantee
        // libobs's own callback contract already gives it.
        let src = unsafe { std::slice::from_raw_parts(plane, frames) };
        let dst = &mut buffer[ch * frames..(ch + 1) * frames];
        for (d, &s) in dst.iter_mut().zip(src) {
            *d = s * volume;
        }
    }
    buffer
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
    // global `TAPS`/`MIX_BUSES` maps under parallel `cargo test`
    // execution.

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
        forward_if_tapped("shot-that-does-not-exist", 48000, 2, 480, &planes, 1.0);
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
        forward_if_tapped("shot-currently-live", 48000, 2, 480, &planes, 0.5);
    }

    #[test]
    fn build_scaled_planar_buffer_applies_volume_to_every_sample() {
        // The actual bug being fixed: the monitor was playing the raw,
        // pre-fader input regardless of where the layer's own volume
        // slider sat — this is the exact arithmetic that must scale it.
        let left = [1.0f32, 0.5, -1.0];
        let right = [0.2f32, -0.4, 0.6];
        let planes: [*const f32; 2] = [left.as_ptr(), right.as_ptr()];
        let buffer = build_scaled_planar_buffer(2, 3, &planes, 0.25);
        assert_eq!(buffer, vec![0.25, 0.125, -0.25, 0.05, -0.1, 0.15]);
    }

    #[test]
    fn build_scaled_planar_buffer_skips_null_planes_without_panicking() {
        let left = [1.0f32, 1.0];
        let planes: [*const f32; 2] = [left.as_ptr(), std::ptr::null()];
        let buffer = build_scaled_planar_buffer(2, 2, &planes, 1.0);
        assert_eq!(buffer, vec![1.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn sum_contributions_adds_sample_by_sample() {
        let a = vec![1.0f32, 0.5, -0.5];
        let b = vec![0.2f32, 0.2, 0.2];
        let mixed = sum_contributions(&[a, b]).expect("two real contributors");
        assert_eq!(mixed, vec![1.2, 0.7, -0.3]);
    }

    #[test]
    fn sum_contributions_truncates_to_the_shortest_contributor() {
        let a = vec![1.0f32, 1.0, 1.0, 1.0];
        let b = vec![0.5f32, 0.5];
        let mixed = sum_contributions(&[a, b]).expect("still real contributors");
        assert_eq!(mixed, vec![1.5, 1.5]);
    }

    #[test]
    fn sum_contributions_is_none_when_nothing_contributed() {
        assert_eq!(sum_contributions(&[]), None);
    }

    #[test]
    fn sum_contributions_is_none_for_all_empty_contributors() {
        assert_eq!(sum_contributions(&[vec![], vec![]]), None);
    }

    #[test]
    fn set_mix_sources_never_leaves_a_half_registered_entry() {
        // Same NDI-runtime-dependent split as `start_never_leaves_a_
        // half_registered_entry` above.
        let names = vec!["shot-a".to_string(), "shot-b".to_string()];
        let result = set_mix_sources("test-mix-bus-lifecycle", &names);
        match result {
            Ok(()) => {
                let sources = mix_bus_sources("test-mix-bus-lifecycle").unwrap();
                assert_eq!(sources, names.into_iter().collect());
                stop_mix_bus("test-mix-bus-lifecycle");
                assert_eq!(mix_bus_sources("test-mix-bus-lifecycle"), None);
            }
            Err(_) => {
                assert_eq!(mix_bus_sources("test-mix-bus-lifecycle"), None);
            }
        }
    }

    #[test]
    fn mix_bus_sources_is_none_for_a_bus_that_was_never_started() {
        assert_eq!(mix_bus_sources("test-mix-bus-untouched"), None);
    }

    #[test]
    fn stop_mix_bus_on_a_never_started_bus_is_a_safe_no_op() {
        stop_mix_bus("test-mix-bus-never-started");
        assert_eq!(mix_bus_sources("test-mix-bus-never-started"), None);
    }
}
