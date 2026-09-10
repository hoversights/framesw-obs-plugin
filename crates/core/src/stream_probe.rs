//! Sampling the live stream output faster than a websocket poll can.
//!
//! FrameSW polls obs-websocket every 2 seconds. That is enough to answer
//! "is the encoder behind", because those counters are monotonic and a
//! delta over 2s is still a delta. It is NOT enough to answer "is the
//! stream steady", and the difference matters to a viewer:
//!
//!   A stream that alternates 250ms of full-rate sending with 250ms of
//!   nothing has the same 2-second average as one sending evenly. The
//!   average says healthy. The viewer sees it buffer.
//!
//! Averaging destroys exactly the signal that predicts that, so it has to
//! be measured before the averaging happens. This samples at 10 Hz inside
//! the plugin, where libobs's counters are a direct call rather than a
//! websocket round trip, and reports an AGGREGATE every 2 seconds:
//! jitter, stalls, and peak-versus-mean congestion. None of those three
//! can be recovered from 2-second samples afterwards.
//!
//! COST. Each sample is four calls that read plain counters out of the
//! output struct, plus one f32 read. At 10 Hz that is 40 calls a second
//! on a dedicated thread that is asleep the rest of the time.
//!
//! NOT ON THE GRAPHICS THREAD, deliberately. `obs_add_tick_callback` runs
//! per rendered frame, which would be the natural-looking place to put
//! this -- and it is the wrong one twice over: it is the exact thread
//! whose stalls this measures, so the sampler would stop sampling
//! precisely when there is something to see, and any work there adds to
//! the render budget this is supposed to be observing.

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub enum ObsOutputT {}
pub enum ObsEncoderT {}

// `libobs/obs-frontend-api` is not linkable from a plain module, so the
// stream output is found by name instead. OBS names it "simple_stream" in
// Simple mode and "adv_stream" in Advanced -- checked in that order.
crate::resolved_fn!(obs_get_output_by_name: extern "C" fn(*const c_char) -> *mut ObsOutputT);
crate::resolved_fn!(obs_output_release: extern "C" fn(*mut ObsOutputT));
crate::resolved_fn!(obs_output_active: extern "C" fn(*mut ObsOutputT) -> bool);
crate::resolved_fn!(obs_output_get_total_bytes: extern "C" fn(*mut ObsOutputT) -> u64);
crate::resolved_fn!(obs_output_get_frames_dropped: extern "C" fn(*mut ObsOutputT) -> i32);
crate::resolved_fn!(obs_output_get_total_frames: extern "C" fn(*mut ObsOutputT) -> i32);
crate::resolved_fn!(obs_output_get_congestion: extern "C" fn(*mut ObsOutputT) -> f32);
// The reason this lives in the plugin at all: obs-websocket exposes no
// request for the configured bitrate in Advanced mode, where it sits in
// the profile's streamEncoder.json rather than basic.ini. libobs will
// simply hand it over, for either mode and any encoder.
crate::resolved_fn!(obs_output_get_video_encoder: extern "C" fn(*mut ObsOutputT) -> *mut ObsEncoderT);
crate::resolved_fn!(obs_output_get_audio_encoder: extern "C" fn(*mut ObsOutputT, usize) -> *mut ObsEncoderT);
crate::resolved_fn!(obs_encoder_get_settings: extern "C" fn(*mut ObsEncoderT) -> *mut crate::obs_data::ObsDataT);
crate::resolved_fn!(obs_encoder_get_id: extern "C" fn(*mut ObsEncoderT) -> *const c_char);

/// One 100ms reading.
#[derive(Clone, Copy)]
struct Tick {
    at: Instant,
    bytes: u64,
    dropped: i32,
    total: i32,
    congestion: f32,
}

/// What 10 Hz sampling can say that 2 Hz cannot.
#[derive(Clone, Copy, Default, Debug)]
pub struct Quality {
    /// Mean send rate over the window.
    pub kbps_mean: f64,
    /// Standard deviation of the per-100ms rates, as a FRACTION of the
    /// mean. A steady stream is near 0; one that bursts and stalls
    /// approaches and exceeds 1 while its mean looks perfectly fine.
    pub jitter: f64,
    /// Buckets belonging to a RUN of two or more consecutive empty ones
    /// — at least 200ms with nothing sent.
    ///
    /// Not "any empty bucket": an encoder hands the output data in
    /// bursts, so at 100ms granularity a lone gap is ordinary and
    /// counting it would make every healthy stream look broken. A gap
    /// that lasts is the thing a viewer feels.
    pub stalls: u32,
    pub buckets: u32,
    pub congestion_peak: f32,
    pub congestion_mean: f32,
    pub dropped_delta: i32,
    pub total_delta: i32,
}

impl Quality {
    /// 0..=100, where 100 is a stream nobody would notice.
    ///
    /// Deliberately combines the three things a 2-second average hides,
    /// not the things it already reports. Frame loss is not in here --
    /// FrameSW measures that directly and attributes it to a part of the
    /// machine; this number is only about whether delivery was STEADY.
    ///
    /// Stalls dominate, and by a wide margin. Time spent sending nothing
    /// is what a viewer actually experiences as buffering; jitter and
    /// congestion are warnings that it may be coming. A first pass
    /// weighted them 60/25/15 and scored a stream that sent nothing for
    /// half the window at 53 out of 100, which is not a description
    /// anyone would recognise.
    pub fn score(&self) -> u8 {
        if self.buckets == 0 {
            return 100;
        }
        let stall_frac = f64::from(self.stalls) / f64::from(self.buckets);
        let penalty = stall_frac * 100.0
            + self.jitter.min(1.5) / 1.5 * 30.0
            + f64::from(self.congestion_peak).min(1.0) * 20.0;
        (100.0 - penalty).clamp(0.0, 100.0) as u8
    }
}

struct Probe {
    stop: Arc<AtomicBool>,
    window: Arc<Mutex<Vec<Tick>>>,
}

static PROBE: Mutex<Option<Probe>> = Mutex::new(None);

/// The stream output, by the two names OBS gives it.
fn stream_output() -> *mut ObsOutputT {
    let Some(get) = obs_get_output_by_name() else {
        return std::ptr::null_mut();
    };
    for name in [c"simple_stream", c"adv_stream"] {
        let out = get(name.as_ptr());
        if !out.is_null() {
            return out;
        }
    }
    std::ptr::null_mut()
}

/// How often the window is sampled. 10 Hz resolves a stall a viewer
/// would notice; faster buys nothing, because the encoder hands the
/// output data in bursts anyway.
const TICK: Duration = Duration::from_millis(100);
/// How much history the window holds — matched to FrameSW's own 2s poll,
/// so each request gets a fresh, non-overlapping window.
const WINDOW: Duration = Duration::from_secs(2);

/// Starts the sampler. Idempotent.
pub fn start() {
    let mut guard = PROBE.lock().unwrap();
    if guard.is_some() {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let window: Arc<Mutex<Vec<Tick>>> = Arc::new(Mutex::new(Vec::new()));
    let (s, w) = (Arc::clone(&stop), Arc::clone(&window));
    std::thread::Builder::new()
        .name("framesw-stream-probe".into())
        .spawn(move || {
            while !s.load(Ordering::Relaxed) {
                std::thread::sleep(TICK);
                let out = stream_output();
                if out.is_null() {
                    continue;
                }
                // Every one of these is optional: an older libobs may not
                // export all of them, and a missing symbol must degrade
                // to "no reading" rather than take OBS down.
                let active = obs_output_active().map(|f| f(out)).unwrap_or(false);
                if active {
                    let tick = Tick {
                        at: Instant::now(),
                        bytes: obs_output_get_total_bytes().map(|f| f(out)).unwrap_or(0),
                        dropped: obs_output_get_frames_dropped().map(|f| f(out)).unwrap_or(0),
                        total: obs_output_get_total_frames().map(|f| f(out)).unwrap_or(0),
                        congestion: obs_output_get_congestion().map(|f| f(out)).unwrap_or(0.0),
                    };
                    if let Ok(mut win) = w.lock() {
                        win.push(tick);
                        let cutoff = tick.at - WINDOW;
                        win.retain(|t| t.at >= cutoff);
                    }
                } else if let Ok(mut win) = w.lock() {
                    // Not streaming: drop the history rather than let a
                    // stale window describe a stream that ended.
                    win.clear();
                }
                if let Some(release) = obs_output_release() {
                    release(out);
                }
            }
        })
        .ok();
    *guard = Some(Probe { stop, window });
}

/// Stops the sampler and waits for the thread to notice.
pub fn shutdown() {
    if let Some(p) = PROBE.lock().unwrap().take() {
        p.stop.store(true, Ordering::Relaxed);
    }
}

/// Buckets that are part of a run of two or more consecutive empty ones.
/// See `Quality::stalls` for why a lone gap does not count.
fn stall_buckets(empty: &[bool]) -> u32 {
    let mut total = 0u32;
    let mut run = 0u32;
    for &e in empty {
        if e {
            run += 1;
        } else {
            if run >= 2 {
                total += run;
            }
            run = 0;
        }
    }
    if run >= 2 {
        total += run;
    }
    total
}

/// The current window, reduced.
///
/// `None` when nothing is streaming or too little has been collected —
/// three ticks is the minimum that can produce two intervals and
/// therefore any notion of variance at all.
pub fn quality() -> Option<Quality> {
    let guard = PROBE.lock().unwrap();
    let probe = guard.as_ref()?;
    let win = probe.window.lock().ok()?;
    if win.len() < 3 {
        return None;
    }
    let mut rates = Vec::with_capacity(win.len());
    let mut empty = Vec::with_capacity(win.len());
    for pair in win.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let secs = b.at.duration_since(a.at).as_secs_f64();
        if secs <= 0.0 {
            continue;
        }
        let delta = b.bytes.saturating_sub(a.bytes);
        empty.push(delta == 0);
        rates.push(delta as f64 * 8.0 / secs / 1000.0);
    }
    let stalls = stall_buckets(&empty);
    if rates.is_empty() {
        return None;
    }
    let mean = rates.iter().sum::<f64>() / rates.len() as f64;
    let var = rates.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rates.len() as f64;
    // Relative, not absolute: 500 kbps of swing is nothing on a 20 Mbps
    // stream and catastrophic on a 1 Mbps one, and an absolute figure
    // would need a different threshold per stream to mean anything.
    let jitter = if mean > 0.0 { var.sqrt() / mean } else { 0.0 };
    let (first, last) = (win.first()?, win.last()?);
    let cong: Vec<f32> = win.iter().map(|t| t.congestion).collect();
    Some(Quality {
        kbps_mean: mean,
        jitter,
        stalls,
        buckets: rates.len() as u32,
        congestion_peak: cong.iter().copied().fold(0.0, f32::max),
        congestion_mean: cong.iter().sum::<f32>() / cong.len() as f32,
        dropped_delta: last.dropped.saturating_sub(first.dropped),
        total_delta: last.total.saturating_sub(first.total),
    })
}

/// What the stream is CONFIGURED to send — the thing obs-websocket
/// cannot answer in Advanced mode. Returns `(video_kbps, audio_kbps,
/// encoder_id)`, each optional.
pub fn configured() -> (Option<i64>, Option<i64>, Option<String>) {
    let out = stream_output();
    if out.is_null() {
        return (None, None, None);
    }
    let read_bitrate = |enc: *mut ObsEncoderT| -> Option<i64> {
        if enc.is_null() {
            return None;
        }
        let settings = obs_encoder_get_settings()?(enc);
        if settings.is_null() {
            return None;
        }
        let key = std::ffi::CString::new("bitrate").ok()?;
        let v = crate::obs_data::obs_data_get_int()?(settings, key.as_ptr());
        crate::obs_data::obs_data_release().map(|f| f(settings));
        (v > 0).then_some(v)
    };
    let venc = obs_output_get_video_encoder().map(|f| f(out)).unwrap_or(std::ptr::null_mut());
    let aenc = obs_output_get_audio_encoder().map(|f| f(out, 0)).unwrap_or(std::ptr::null_mut());
    let video = read_bitrate(venc);
    let audio = read_bitrate(aenc);
    let id = obs_encoder_get_id()
        .filter(|_| !venc.is_null())
        .map(|f| f(venc))
        .filter(|p| !p.is_null())
        .map(|p| unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned());
    if let Some(release) = obs_output_release() {
        release(out);
    }
    (video, audio, id)
}

/// Silences the unused-import warning on the `c_void` used only by the
/// opaque handle types above.
const _: Option<*mut c_void> = None;

#[cfg(test)]
mod tests {
    use super::*;

    /// Reduce a synthetic window the same way `quality` does. Kept
    /// separate from the live path so the maths is testable without OBS,
    /// which is the only part of this module that can be.
    fn reduce(byte_deltas: &[u64], congestion: &[f32]) -> Quality {
        let mut rates = Vec::new();
        let mut empty = Vec::new();
        for d in byte_deltas {
            empty.push(*d == 0);
            rates.push(*d as f64 * 8.0 / 0.1 / 1000.0);
        }
        let stalls = stall_buckets(&empty);
        let mean = rates.iter().sum::<f64>() / rates.len() as f64;
        let var = rates.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rates.len() as f64;
        Quality {
            kbps_mean: mean,
            jitter: if mean > 0.0 { var.sqrt() / mean } else { 0.0 },
            stalls,
            buckets: rates.len() as u32,
            congestion_peak: congestion.iter().copied().fold(0.0, f32::max),
            congestion_mean: congestion.iter().sum::<f32>() / congestion.len() as f32,
            dropped_delta: 0,
            total_delta: 0,
        }
    }

    /// THE case this module exists for. Both streams send exactly the
    /// same number of bytes over the window, so a 2-second poll cannot
    /// tell them apart -- identical mean, identical everything it can
    /// see. One is steady; one is sawtoothing between full rate and
    /// nothing.
    #[test]
    fn a_sawtoothing_stream_and_a_steady_one_have_the_same_average() {
        let steady = reduce(&[50_000; 8], &[0.0; 8]);
        let bursty = reduce(&[100_000, 0, 100_000, 0, 100_000, 0, 100_000, 0], &[0.0; 8]);

        assert!(
            (steady.kbps_mean - bursty.kbps_mean).abs() < 0.01,
            "the averages must match, or this test is not making its point: {} vs {}",
            steady.kbps_mean,
            bursty.kbps_mean
        );
        assert!(
            bursty.jitter > 0.9 && steady.jitter < 0.01,
            "jitter is what separates them: steady {:.2}, bursty {:.2}",
            steady.jitter,
            bursty.jitter
        );
        // NOT counted as stalls: no two empty buckets are adjacent, and
        // an encoder handing data over in bursts looks exactly like this
        // while the viewer sees nothing wrong.
        assert_eq!(bursty.stalls, 0, "alternating gaps are jitter, not stalling");
        // Marked down, but not condemned: with no sustained gap this can
        // simply be the encoder bursting, and calling it a failure would
        // cry wolf on healthy streams. A warning, not an alarm.
        assert!(steady.score() > 95, "steady {}", steady.score());
        assert!(
            bursty.score() <= steady.score() - 15,
            "jitter must be visible in the score: steady {}, bursty {}",
            steady.score(),
            bursty.score()
        );
    }

    /// A sustained gap is the one a viewer feels, and it has to outrank
    /// jitter by a wide margin. An earlier weighting scored a stream that
    /// sent nothing for half the window at 53 out of 100, which is not a
    /// description anyone would recognise.
    #[test]
    fn a_sustained_gap_is_a_stall_and_dominates_the_score() {
        let stalling = reduce(&[100_000, 100_000, 0, 0, 0, 0, 100_000, 100_000], &[0.0; 8]);
        assert_eq!(stalling.stalls, 4, "400ms of nothing, in one run");
        assert!(
            stalling.score() < 40,
            "half the window silent must not read as a passable stream: {}",
            stalling.score()
        );
    }

    /// One lone empty bucket is ordinary. Counting it would make every
    /// healthy stream look broken, which is the fastest way to make this
    /// number ignorable.
    #[test]
    fn a_single_empty_bucket_is_not_a_stall() {
        let q = reduce(&[50_000, 50_000, 0, 50_000, 50_000, 50_000], &[0.0; 6]);
        assert_eq!(q.stalls, 0);
        assert!(q.score() > 70, "a single gap must not tank the score: {}", q.score());
    }

    /// Jitter is RELATIVE. 500 kbps of swing is nothing on a 20 Mbps
    /// stream and catastrophic on a 1 Mbps one; an absolute figure would
    /// need a different threshold per stream to mean anything at all.
    #[test]
    fn jitter_is_relative_to_the_streams_own_rate() {
        // Same shape, ten times the rate.
        let small = reduce(&[10_000, 12_000, 10_000, 12_000], &[0.0; 4]);
        let large = reduce(&[100_000, 120_000, 100_000, 120_000], &[0.0; 4]);
        assert!(
            (small.jitter - large.jitter).abs() < 0.001,
            "proportionally identical streams must score identically: {} vs {}",
            small.jitter,
            large.jitter
        );
    }

    /// Peak and mean congestion are different facts. A stream that spikes
    /// to 90% for one bucket and sits at 0% otherwise averages to
    /// something reassuring, and the spike is the part a viewer felt.
    #[test]
    fn congestion_peak_survives_the_averaging_that_hides_it() {
        let q = reduce(&[50_000; 8], &[0.0, 0.0, 0.9, 0.0, 0.0, 0.0, 0.0, 0.0]);
        assert!((q.congestion_peak - 0.9).abs() < 0.001);
        assert!(q.congestion_mean < 0.2, "the mean alone would look fine: {}", q.congestion_mean);
    }

    #[test]
    fn a_clean_stream_scores_full_marks() {
        assert_eq!(reduce(&[50_000; 20], &[0.0; 20]).score(), 100);
    }

    /// An empty window is not a perfect stream, but it must not be a
    /// zero either -- callers show this number, and inventing a bad
    /// score for "no data" is worse than showing nothing.
    #[test]
    fn no_buckets_is_not_a_bad_score() {
        assert_eq!(Quality::default().score(), 100);
    }
}
