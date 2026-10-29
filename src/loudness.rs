//! Gated loudness (ITU-R BS.1770-4 / EBU R 128) for the input probe: how
//! loud a source is while it makes sound, in LUFS, so FrameSW can set the
//! trim that lands every sound on one standard level (FrameSW's
//! MIC_CHECK_PLAN.md, "The input stage", 2026-10-29).
//!
//! K-weighting (a high shelf, then a high-pass) per channel; mean square
//! per 100 ms; 400 ms blocks overlapping by 75 %; an absolute gate at −70
//! LUFS and a relative gate 10 LU under the loudness of what passed it.
//! The gates are the point: the pauses between words, or a game's quiet
//! menu between sounds, don't pull the figure down.
//!
//! Every channel is weighted 1.0, the standard's weight for left, right
//! and centre, which is what OBS's stereo mix is. The filter coefficients
//! for any sample rate follow libebur128's derivation of the standard's
//! 48 kHz filters, which they reproduce (tested below).

/// A biquad, direct form I.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad {
    fn run(&mut self, x0: f64) -> f64 {
        let y0 = self.b[0] * x0 + self.b[1] * self.x[0] + self.b[2] * self.x[1] - self.a[0] * self.y[0] - self.a[1] * self.y[1];
        self.x = [x0, self.x[0]];
        self.y = [y0, self.y[0]];
        y0
    }
}

/// The two K-weighting stages for `fs`.
fn k_weighting(fs: f64) -> (Biquad, Biquad) {
    let pi = std::f64::consts::PI;
    // Stage 1: the high shelf.
    let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (pi * f0 / fs).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad {
        b: [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        ..Biquad::default()
    };
    // Stage 2: the high-pass.
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (pi * f0 / fs).tan();
    let a0 = 1.0 + k / q + k * k;
    let high_pass = Biquad {
        b: [1.0, -2.0, 1.0],
        a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        ..Biquad::default()
    };
    (shelf, high_pass)
}

const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const RELATIVE_GATE_LU: f64 = -10.0;

fn lufs(mean_square: f64) -> f64 {
    -0.691 + 10.0 * mean_square.log10()
}

pub struct Loudness {
    filters: Vec<(Biquad, Biquad)>,
    /// Samples in 100 ms.
    step: usize,
    /// Per channel: the K-weighted sum of squares of the 100 ms so far.
    sums: Vec<f64>,
    filled: usize,
    /// Each finished 100 ms: the channels' mean squares, summed.
    steps: Vec<f64>,
}

impl Loudness {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let fs = f64::from(sample_rate.max(8000));
        Loudness {
            filters: vec![k_weighting(fs); channels],
            step: (fs / 10.0).round() as usize,
            sums: vec![0.0; channels],
            filled: 0,
            steps: Vec::new(),
        }
    }

    /// One packet: a plane per channel, all the same length; `None` is a
    /// channel with no data (silence).
    pub fn push(&mut self, planes: &[Option<&[f32]>]) {
        let frames = planes.iter().flatten().map(|p| p.len()).max().unwrap_or(0);
        for i in 0..frames {
            for (c, (shelf, high_pass)) in self.filters.iter_mut().enumerate() {
                let x = planes.get(c).copied().flatten().and_then(|p| p.get(i)).map_or(0.0, |&v| f64::from(v));
                let y = high_pass.run(shelf.run(x));
                self.sums[c] += y * y;
            }
            self.filled += 1;
            if self.filled == self.step {
                let n = self.step as f64;
                self.steps.push(self.sums.iter().map(|s| s / n).sum());
                self.sums.iter_mut().for_each(|s| *s = 0.0);
                self.filled = 0;
            }
        }
    }

    /// The gated loudness in LUFS, and the seconds of sound that counted
    /// (the blocks past both gates, at 100 ms each). `None` before a full
    /// 400 ms block, or when nothing rose above −70 LUFS.
    pub fn result(&self) -> Option<(f64, f64)> {
        let blocks = self.blocks();
        let loud: Vec<f64> = blocks.iter().copied().filter(|&z| z > 0.0 && lufs(z) > ABSOLUTE_GATE_LUFS).collect();
        if loud.is_empty() {
            return None;
        }
        let gate = lufs(loud.iter().sum::<f64>() / loud.len() as f64) + RELATIVE_GATE_LU;
        let kept: Vec<f64> = loud.into_iter().filter(|&z| lufs(z) > gate).collect();
        if kept.is_empty() {
            return None;
        }
        Some((lufs(kept.iter().sum::<f64>() / kept.len() as f64), kept.len() as f64 * 0.1))
    }

    /// The spread, ungated: the loudness of the quietest blocks (the 10th
    /// percentile, the floor) and of the loud part (the 95th). A room is
    /// steady, so the two sit close; speech and most programme rise and
    /// fall. The gates alone can't tell a room from sound: a room at −47
    /// LUFS passes the −70 gate (measured 2026-10-29). Silence reads −100.
    /// `None` before a full 400 ms block.
    pub fn spread(&self) -> Option<(f64, f64)> {
        let mut levels: Vec<f64> = self.blocks().into_iter().map(|z| if z > 0.0 { lufs(z).max(-100.0) } else { -100.0 }).collect();
        if levels.is_empty() {
            return None;
        }
        levels.sort_by(f64::total_cmp);
        let at = |p: f64| levels[((levels.len() - 1) as f64 * p).round() as usize];
        Some((at(0.10), at(0.95)))
    }

    /// Each 400 ms block's mean square, overlapping by 75 %.
    fn blocks(&self) -> Vec<f64> {
        self.steps.windows(4).map(|w| w.iter().sum::<f64>() / 4.0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(fs: u32, freq: f64, amplitude: f32, seconds: f64) -> Vec<f32> {
        let n = (f64::from(fs) * seconds) as usize;
        (0..n)
            .map(|i| amplitude * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(fs)).sin() as f32)
            .collect()
    }

    fn measure(fs: u32, planes: &[Option<&[f32]>]) -> Option<(f64, f64)> {
        let mut m = Loudness::new(fs, planes.len());
        // In packets, as OBS hands them over.
        let len = planes.iter().flatten().map(|p| p.len()).max().unwrap_or(0);
        for start in (0..len).step_by(1024) {
            let end = (start + 1024).min(len);
            let chunk: Vec<Option<&[f32]>> = planes.iter().map(|p| p.map(|p| &p[start..end])).collect();
            m.push(&chunk);
        }
        m.result()
    }

    /// The standard's 48 kHz coefficients (BS.1770-4, Table 1 and 2).
    #[test]
    fn the_48k_filters_are_the_standards() {
        let (shelf, high_pass) = k_weighting(48000.0);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-6;
        assert!(close(shelf.b[0], 1.53512485958697), "{}", shelf.b[0]);
        assert!(close(shelf.b[1], -2.69169618940638), "{}", shelf.b[1]);
        assert!(close(shelf.b[2], 1.19839281085285), "{}", shelf.b[2]);
        assert!(close(shelf.a[0], -1.69065929318241), "{}", shelf.a[0]);
        assert!(close(shelf.a[1], 0.73248077421585), "{}", shelf.a[1]);
        assert!(close(high_pass.a[0], -1.99004745483398), "{}", high_pass.a[0]);
        assert!(close(high_pass.a[1], 0.99007225036621), "{}", high_pass.a[1]);
    }

    /// BS.1770: a 0 dBFS 997 Hz sine in one channel reads −3.01 LUFS.
    #[test]
    fn a_full_scale_sine_in_one_channel_reads_minus_3() {
        let left = sine(48000, 997.0, 1.0, 3.0);
        let silent = vec![0.0f32; left.len()];
        let (l, secs) = measure(48000, &[Some(&left), Some(&silent)]).unwrap();
        assert!((l - -3.01).abs() < 0.05, "{l}");
        assert!(secs > 2.5, "{secs}");
    }

    /// −20 dBFS in both channels: 3 dB louder than one channel.
    #[test]
    fn minus_20_in_both_channels_reads_minus_20() {
        let s = sine(44100, 997.0, 0.1, 3.0);
        let (l, _) = measure(44100, &[Some(&s), Some(&s)]).unwrap();
        assert!((l - -20.0).abs() < 0.05, "{l}");
    }

    /// The point of the gates: sound with long silences reads as the sound,
    /// not as the average with the silence (which would be −22 here). The
    /// blocks straddling each tone's edges still count, at a quarter to
    /// three quarters of its energy, as the standard has it: with 1 s tones
    /// they pulled the figure to −21.0, so the tones are 5 s.
    #[test]
    fn silence_between_sounds_does_not_lower_it() {
        let tone = sine(48000, 997.0, 0.1, 5.0);
        let mut gappy = Vec::new();
        for _ in 0..3 {
            gappy.extend_from_slice(&tone);
            gappy.extend(std::iter::repeat_n(0.0f32, 48000 * 3));
        }
        let (l, secs) = measure(48000, &[Some(&gappy), Some(&gappy)]).unwrap();
        assert!((l - -20.0).abs() < 0.3, "{l}");
        assert!(secs < 16.5, "{secs}");
    }

    /// A steady sound has no spread; sound with silences between has the
    /// silence as its floor and the sound as its top.
    #[test]
    fn the_spread_tells_steady_from_rising_and_falling() {
        let steady = sine(48000, 997.0, 0.01, 4.0);
        let (floor, top) = measure_spread(48000, &[Some(&steady), Some(&steady)]).unwrap();
        assert!((top - floor).abs() < 0.5, "{floor} {top}");
        assert!((top - -40.0).abs() < 0.5, "{top}");

        let tone = sine(48000, 997.0, 0.1, 2.0);
        let mut gappy = Vec::new();
        for _ in 0..3 {
            gappy.extend_from_slice(&tone);
            gappy.extend(std::iter::repeat_n(0.0f32, 48000 * 2));
        }
        let (floor, top) = measure_spread(48000, &[Some(&gappy), Some(&gappy)]).unwrap();
        assert_eq!(floor, -100.0);
        assert!((top - -20.0).abs() < 0.5, "{top}");
    }

    fn measure_spread(fs: u32, planes: &[Option<&[f32]>]) -> Option<(f64, f64)> {
        let mut m = Loudness::new(fs, planes.len());
        let len = planes.iter().flatten().map(|p| p.len()).max().unwrap_or(0);
        for start in (0..len).step_by(1024) {
            let end = (start + 1024).min(len);
            let chunk: Vec<Option<&[f32]>> = planes.iter().map(|p| p.map(|p| &p[start..end])).collect();
            m.push(&chunk);
        }
        m.spread()
    }

    #[test]
    fn silence_and_short_input_read_nothing() {
        let quiet = vec![0.0f32; 48000];
        assert!(measure(48000, &[Some(&quiet), None]).is_none());
        let short = sine(48000, 997.0, 0.5, 0.3);
        assert!(measure(48000, &[Some(&short)]).is_none());
    }
}
