//! Linear resampler used between the mic sample rate and the speaker rate.
//!
//! The speaker callback only copies from its ring. Rate conversion happens on
//! the bridge thread that reads the data stream.

#[derive(Debug)]
pub struct Resampler {
    in_rate: u32,
    out_rate: u32,
    phase: f64,
    prev: Option<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self {
            in_rate,
            out_rate,
            phase: 0.0,
            prev: None,
        }
    }

    pub fn set_rates(&mut self, in_rate: u32, out_rate: u32) {
        if in_rate != self.in_rate || out_rate != self.out_rate {
            self.in_rate = in_rate;
            self.out_rate = out_rate;
            self.phase = 0.0;
            self.prev = None;
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() || self.in_rate == 0 || self.out_rate == 0 {
            return Vec::new();
        }
        if self.in_rate == self.out_rate {
            return input.to_vec();
        }
        let mut data = Vec::with_capacity(input.len() + 1);
        if let Some(prev) = self.prev {
            data.push(prev);
        }
        data.extend_from_slice(input);
        self.prev = input.last().copied();
        if data.len() < 2 {
            return Vec::new();
        }
        let step = self.in_rate as f64 / self.out_rate as f64;
        let end = (data.len() - 1) as f64;
        let mut out = Vec::new();
        while self.phase < end {
            let index = self.phase as usize;
            let frac = (self.phase - index as f64) as f32;
            let left = data[index];
            let right = data[index + 1];
            out.push(left + (right - left) * frac);
            self.phase += step;
        }
        self.phase -= (data.len() - 1) as f64;
        if self.phase < 0.0 {
            self.phase = 0.0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_rates_copy() {
        let mut resampler = Resampler::new(48_000, 48_000);
        assert_eq!(resampler.process(&[0.0, 0.5, -0.5]), vec![0.0, 0.5, -0.5]);
    }

    #[test]
    fn upsample_grows_the_buffer() {
        let mut resampler = Resampler::new(8_000, 8_000);
        resampler.set_rates(16_000, 48_000);
        let out = resampler.process(&[0.0, 1.0, 0.0, -1.0]);
        assert!(out.len() > 4, "got {}", out.len());
    }
}
