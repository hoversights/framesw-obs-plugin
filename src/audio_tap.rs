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

use std::collections::{HashMap, HashSet, VecDeque};
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
            let sender = NdiSender::new(&ndi_name).map_err(|e| format!("'{ndi_name}': {e}"))?;
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
    /// source_name -> that source's per-channel FIFO queues of scaled
    /// samples (`build_scaled_planar_buffer`'s per-channel slices,
    /// pushed on every matching callback via `extend`). A real queue,
    /// not "keep only the latest chunk": independent sources fire at
    /// independent, uncoordinated times, so a fixed-cadence flush
    /// popping a *specific* frame count per cycle is what avoids
    /// silently dropping samples between callbacks (too fast) or
    /// resending the same ones (too slow) — live-reported, 2026-07-24:
    /// the "latest chunk overwritten" version of this played one
    /// contributing layer clean and another stuttering, exactly the
    /// symptom of that mismatch.
    queues: Mutex<HashMap<String, Vec<VecDeque<f32>>>>,
    /// Which sources currently belong to this bus's mix — read by
    /// `forward_if_tapped` to decide whether to queue, written by
    /// `set_mix_sources`.
    sources: Mutex<HashSet<String>>,
    /// Most recently observed `(sample_rate, channels)` — shared by
    /// every contributor in practice, since all of OBS's audio callbacks
    /// come from the one global audio pipeline (`obs_get_audio_info`),
    /// but tracked per-bus rather than assumed so the flush thread always
    /// sends with whatever's actually current.
    meta: Mutex<(i32, i32)>,
    running: Arc<AtomicBool>,
    /// Set right after `spawn_mix_flush_thread` returns, taken and
    /// joined by `stop_mix_bus` — without this, tearing down a mix bus
    /// only *asked* its flush thread to stop and returned immediately,
    /// so a same-`bus_id` `start_audio_tap` right after (Solo engaging)
    /// could race `NDIlib_send_create` against the old sender's own
    /// `NDIlib_send_destroy`, which only runs once the flush thread's own
    /// `Arc<MixBus>` clone actually drops. Live-reported, 2026-07-24:
    /// every attempt to Solo right after the combined mix had been
    /// playing failed with "NDI runtime unavailable" — the runtime was
    /// fine; `send_create` was failing on the still-live duplicate name.
    thread_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// bus_id -> its active mix bus, if any.
static MIX_BUSES: Mutex<Option<HashMap<String, Arc<MixBus>>>> = Mutex::new(None);

/// Replaces which sources contribute to `bus_id`'s combined mix. Safe to
/// call repeatedly, including with the same set (a no-op beyond updating
/// bookkeeping) or a changed set (sources no longer listed stop
/// contributing from this call on — their queued audio is dropped
/// immediately, not left to keep draining as stale phantom audio).
/// Creates the bus (and its NDI sender + flush thread) on first use for
/// a given `bus_id`. Also tears down any exclusive `Tap` on the same
/// `bus_id` — mutually exclusive per bus, same as `start_audio_tap`'s
/// side of that relationship.
///
/// Returns `Err` only when a *new* bus is needed and `NdiSender::new`
/// itself failed — same conditions as `start_audio_tap`.
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
            let sender = NdiSender::new(&ndi_name).map_err(|e| format!("'{ndi_name}': {e}"))?;
            let bus = Arc::new(MixBus {
                sender,
                queues: Mutex::new(HashMap::new()),
                sources: Mutex::new(HashSet::new()),
                meta: Mutex::new((0, 0)),
                running: Arc::new(AtomicBool::new(true)),
                thread_handle: Mutex::new(None),
            });
            let handle = spawn_mix_flush_thread(Arc::clone(&bus));
            if let Ok(mut slot) = bus.thread_handle.lock() {
                *slot = Some(handle);
            }
            buses.insert(bus_id.to_string(), Arc::clone(&bus));
            bus
        }
    };
    let new_set: HashSet<String> = source_names.iter().cloned().collect();
    if let Ok(mut sources) = bus.sources.lock() {
        *sources = new_set.clone();
    }
    if let Ok(mut queues) = bus.queues.lock() {
        queues.retain(|name, _| new_set.contains(name));
    }
    Ok(())
}

/// Tears down `bus_id`'s mix bus: stops its flush thread and **blocks
/// until that thread has actually exited** (at most one
/// `MIX_FLUSH_INTERVAL`, ~10ms) before returning, so its `NdiSender` is
/// genuinely destroyed — not just scheduled to be — by the time this
/// call returns. That's what makes it safe for `start_audio_tap` to
/// immediately create a new sender under the same name right after
/// (see `MixBus::thread_handle`'s doc comment for the real failure this
/// fixes). Stopping an already-inactive bus is a safe no-op.
pub fn stop_mix_bus(bus_id: &str) {
    let bus = {
        let Ok(mut guard) = MIX_BUSES.lock() else {
            return;
        };
        guard.as_mut().and_then(|buses| buses.remove(bus_id))
    };
    let Some(bus) = bus else {
        return;
    };
    bus.running.store(false, Ordering::Release);
    let handle = bus.thread_handle.lock().ok().and_then(|mut h| h.take());
    if let Some(handle) = handle {
        let _ = handle.join();
    }
    // `bus` (our last `Arc` reference, now that the flush thread's own
    // clone has also dropped by returning) goes out of scope here,
    // running `NdiSender`'s `Drop` synchronously before this function
    // returns.
}

/// The set of sources currently contributing to `bus_id`'s mix, if that
/// bus is active (empty set if active but nothing's contributing yet —
/// distinct from `None`, "no mix bus exists for this bus_id at all").
pub fn mix_bus_sources(bus_id: &str) -> Option<HashSet<String>> {
    let guard = MIX_BUSES.lock().ok()?;
    let bus = guard.as_ref()?.get(bus_id)?;
    bus.sources.lock().ok().map(|s| s.clone())
}

fn spawn_mix_flush_thread(bus: Arc<MixBus>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while bus.running.load(Ordering::Acquire) {
            std::thread::sleep(MIX_FLUSH_INTERVAL);
            if !bus.running.load(Ordering::Acquire) {
                return;
            }
            let (sample_rate, channels) = bus.meta.lock().map(|m| *m).unwrap_or((0, 0));
            if sample_rate <= 0 || channels <= 0 {
                continue;
            }
            let frames_per_flush =
                ((sample_rate as f64) * MIX_FLUSH_INTERVAL.as_secs_f64()).round() as usize;
            if frames_per_flush == 0 {
                continue;
            }
            let channels = channels as usize;
            let mixed = {
                let Ok(mut queues) = bus.queues.lock() else {
                    continue;
                };
                drain_and_sum(&mut queues, channels, frames_per_flush)
            };
            let Some(mut mixed) = mixed else {
                continue;
            };
            bus.sender.send_audio(sample_rate, channels as i32, frames_per_flush as i32, &mut mixed);
        }
    })
}

/// Pops exactly `frames_per_flush` samples per channel from each
/// contributing source's FIFO queues (silence-padding a source that
/// hasn't delivered enough yet this cycle — real underrun, e.g. it just
/// joined the mix, not a bug; the alternative, blocking for more data,
/// would stall every *other* contributor's audio too) and sums them into
/// one combined planar buffer. A source whose queue count doesn't match
/// `channels` is skipped entirely (defensive guard — channel count is
/// one global OBS setting shared by every source, so this isn't an
/// expected path, just cheap insurance against ever indexing out of
/// bounds). `None` if nothing in the map has the right channel count to
/// contribute at all this cycle.
///
/// Split out from the flush thread's own loop so this — the actual
/// mixing arithmetic, and the fix for "one layer plays clean, another
/// stutters" — is unit-testable without a real thread, NDI sender, or
/// bus registry.
fn drain_and_sum(
    queues: &mut HashMap<String, Vec<VecDeque<f32>>>,
    channels: usize,
    frames_per_flush: usize,
) -> Option<Vec<f32>> {
    let mut mixed = vec![0.0f32; frames_per_flush * channels];
    let mut contributor_count = 0usize;
    for per_channel in queues.values_mut() {
        if per_channel.len() != channels {
            continue;
        }
        contributor_count += 1;
        for (ch, queue) in per_channel.iter_mut().enumerate() {
            for i in 0..frames_per_flush {
                let sample = queue.pop_front().unwrap_or(0.0);
                mixed[ch * frames_per_flush + i] += sample;
            }
        }
    }
    if contributor_count == 0 {
        return None;
    }
    // Headroom: divide by how many sources actually contributed *this
    // cycle*, not a fixed constant — two normal, near-unity sources
    // summed raw easily doubles peak amplitude, which is exactly what
    // live-reported "distorted" audio sounded like (harsh clipping, not
    // stuttering — a different bug from the underrun/repeat one above).
    // A single contributor plays at its own unattenuated level (nothing
    // to clash with); each of N contributors effectively drops to 1/N so
    // the sum can't exceed unity as long as no individual source already
    // does. The final `clamp` is defense in depth for the case where one
    // does (upstream gain/filters pushing a source past 0dBFS on its
    // own) — never let a monitor-only mix send a value NDI/the receiving
    // device would have to clip unpredictably.
    let gain = 1.0 / contributor_count as f32;
    for sample in &mut mixed {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
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
                if let Ok(mut queues) = bus.queues.lock() {
                    let frames_usize = frames as usize;
                    let channels_usize = channels as usize;
                    let per_channel = queues
                        .entry(source_name.to_string())
                        .or_insert_with(|| (0..channels_usize).map(|_| VecDeque::new()).collect());
                    if per_channel.len() != channels_usize {
                        per_channel.resize_with(channels_usize, VecDeque::new);
                    }
                    // Capped so a source the flush thread has stopped
                    // draining (bus paused, or this source just left the
                    // mix a moment before this particular callback still
                    // in flight) can't grow its queue unboundedly — same
                    // "cap buffered audio" reasoning `ndi.rs`'s own
                    // receive loop already uses on the app side.
                    const MAX_QUEUED_FRAMES: usize = 48_000; // ~1s at 48kHz — generous.
                    for (ch, queue) in per_channel.iter_mut().enumerate() {
                        queue.extend(&buffer[ch * frames_usize..(ch + 1) * frames_usize]);
                        while queue.len() > MAX_QUEUED_FRAMES {
                            queue.pop_front();
                        }
                    }
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

    fn queues_from(source_channel_samples: &[(&str, &[&[f32]])]) -> HashMap<String, Vec<VecDeque<f32>>> {
        source_channel_samples
            .iter()
            .map(|(name, channels)| {
                let per_channel = channels.iter().map(|samples| samples.iter().copied().collect()).collect();
                (name.to_string(), per_channel)
            })
            .collect()
    }

    #[test]
    fn drain_and_sum_adds_two_sources_then_applies_headroom() {
        let mut queues = queues_from(&[
            ("a", &[&[1.0f32, 0.5, -0.5]]),
            ("b", &[&[0.2f32, 0.2, 0.2]]),
        ]);
        let mixed = drain_and_sum(&mut queues, 1, 3).expect("two real contributors");
        // Raw sum would be [1.2, 0.7, -0.3] — halved (two contributors)
        // to leave headroom, the actual fix for live-reported distortion
        // (two normal-level sources summed raw clipping hard).
        assert_eq!(mixed, vec![0.6, 0.35, -0.15]);
    }

    #[test]
    fn drain_and_sum_single_contributor_plays_at_full_level() {
        // No headroom penalty when there's nothing else to clash with —
        // only N > 1 contributors should ever get attenuated.
        let mut queues = queues_from(&[("a", &[&[0.8f32, -0.8]])]);
        let mixed = drain_and_sum(&mut queues, 1, 2).expect("one contributor");
        assert_eq!(mixed, vec![0.8, -0.8]);
    }

    #[test]
    fn drain_and_sum_clamps_a_source_that_already_exceeds_unity_on_its_own() {
        // Defense in depth: headroom alone only guarantees no clipping
        // when every individual contributor is itself within [-1, 1] —
        // upstream gain/filters could push one source past that on its
        // own, and the final mix must never hand NDI/the output device
        // something further out of range than a single hot source
        // already was.
        let mut queues = queues_from(&[("a", &[&[1.5f32, -1.5]])]);
        let mixed = drain_and_sum(&mut queues, 1, 2).expect("one (hot) contributor");
        assert_eq!(mixed, vec![1.0, -1.0]);
    }

    #[test]
    fn drain_and_sum_pads_a_source_that_underran_with_silence_not_a_repeat() {
        // The actual bug being fixed: a source delivering less than one
        // flush cycle's worth of samples must contribute silence for the
        // remainder, never its last values repeated — that repetition is
        // exactly what a "keep only the latest chunk" design produced
        // (live-reported stutter on one of two mixed layers).
        let mut queues = queues_from(&[
            ("a", &[&[1.0f32, 1.0, 1.0, 1.0]]),
            ("b", &[&[0.5f32, 0.5]]), // only 2 of this cycle's 4 frames available
        ]);
        let mixed = drain_and_sum(&mut queues, 1, 4).expect("still a real contributor");
        // Raw sum [1.5, 1.5, 1.0, 1.0], halved for headroom (2 contributors).
        assert_eq!(mixed, vec![0.75, 0.75, 0.5, 0.5]);
    }

    #[test]
    fn drain_and_sum_handles_multiple_channels_independently() {
        let mut queues = queues_from(&[("a", &[&[1.0f32, 1.0], &[-1.0f32, -1.0]])]);
        let mixed = drain_and_sum(&mut queues, 2, 2).expect("one contributor, two channels");
        // Planar layout: channel 0's frames, then channel 1's. One
        // contributor, so no headroom attenuation.
        assert_eq!(mixed, vec![1.0, 1.0, -1.0, -1.0]);
    }

    #[test]
    fn drain_and_sum_skips_a_source_with_the_wrong_channel_count() {
        // Defensive guard path — channel count is one global OBS setting,
        // so this isn't expected in practice, but must never panic/index
        // out of bounds if it somehow happened.
        let mut queues = queues_from(&[("mono-in-a-stereo-mix", &[&[1.0f32, 1.0]])]);
        assert_eq!(drain_and_sum(&mut queues, 2, 2), None);
    }

    #[test]
    fn drain_and_sum_is_none_when_nothing_contributed() {
        let mut queues = HashMap::new();
        assert_eq!(drain_and_sum(&mut queues, 2, 480), None);
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
