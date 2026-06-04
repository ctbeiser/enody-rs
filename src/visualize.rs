use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rustfft::{num_complex::Complex, FftPlanner};
use std::sync::{Arc, Mutex};

/// Real-time audio analyzer that captures microphone input and produces
/// per-band energy values via FFT analysis.
pub struct AudioAnalyzer {
    energies: Arc<Mutex<Vec<f32>>>,
    _stream: cpal::Stream,
}

impl AudioAnalyzer {
    /// Create a new AudioAnalyzer that splits the spectrum into `num_bands` bands.
    ///
    /// `fft_size` controls the FFT window size (must be a power of 2).
    /// `min_freq` and `max_freq` define the frequency range mapped to bands.
    /// `gain` scales the raw energy values.
    pub fn new(
        num_bands: usize,
        fft_size: usize,
        min_freq: f32,
        max_freq: f32,
        gain: f32,
    ) -> Result<Self, crate::Error> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| crate::Error::Debug("No audio input device found".into()))?;

        let config = device
            .default_input_config()
            .map_err(|e| crate::Error::Debug(format!("Failed to get input config: {}", e)))?;

        let sample_rate = config.sample_rate().0 as f32;
        let channels = config.channels() as usize;

        log::info!(
            "Audio device: {:?}, sample rate: {}, channels: {}",
            device.name().unwrap_or_default(),
            sample_rate,
            channels
        );

        // Compute logarithmically-spaced band boundaries
        let band_edges = log_band_edges(num_bands, min_freq, max_freq);
        let bin_hz = sample_rate / fft_size as f32;

        let energies: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(vec![0.0; num_bands]));
        let energies_writer = energies.clone();

        // Ring buffer for mono-mixed PCM samples
        let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::with_capacity(fft_size)));
        let buffer_writer = buffer.clone();

        let stream = device
            .build_input_stream(
                &config.into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let mut buf = buffer_writer.lock().unwrap();

                    // Mono-mix and accumulate
                    for frame in data.chunks(channels) {
                        let mono: f32 = frame.iter().sum::<f32>() / channels as f32;
                        buf.push(mono);
                    }

                    // Run FFT when we have enough samples
                    if buf.len() >= fft_size {
                        let samples: Vec<f32> = buf.drain(..fft_size).collect();

                        // Apply Hann window and convert to complex
                        let mut fft_input: Vec<Complex<f32>> = samples
                            .iter()
                            .enumerate()
                            .map(|(i, &s)| {
                                let window = 0.5
                                    * (1.0
                                        - (2.0 * std::f32::consts::PI * i as f32
                                            / (fft_size as f32 - 1.0))
                                            .cos());
                                Complex::new(s * window, 0.0)
                            })
                            .collect();

                        let mut planner = FftPlanner::new();
                        let fft = planner.plan_fft_forward(fft_size);
                        fft.process(&mut fft_input);

                        // Compute energy per band
                        let mut band_values = vec![0.0f32; num_bands];
                        let nyquist_bins = fft_size / 2;

                        for band in 0..num_bands {
                            let lo_bin =
                                ((band_edges[band] / bin_hz) as usize).min(nyquist_bins - 1);
                            let hi_bin =
                                ((band_edges[band + 1] / bin_hz) as usize).min(nyquist_bins);

                            if hi_bin <= lo_bin {
                                continue;
                            }

                            let mut sum = 0.0f32;
                            for bin in lo_bin..hi_bin {
                                sum += fft_input[bin].norm_sqr();
                            }
                            // Average energy across bins, apply gain, normalize
                            let avg = (sum / (hi_bin - lo_bin) as f32).sqrt();
                            band_values[band] = (avg * gain).clamp(0.0, 1.0);
                        }

                        if let Ok(mut e) = energies_writer.lock() {
                            *e = band_values;
                        }
                    }
                },
                move |err| {
                    log::error!("Audio stream error: {}", err);
                },
                None,
            )
            .map_err(|e| crate::Error::Debug(format!("Failed to build audio stream: {}", e)))?;

        stream
            .play()
            .map_err(|e| crate::Error::Debug(format!("Failed to start audio stream: {}", e)))?;

        Ok(Self {
            energies,
            _stream: stream,
        })
    }

    /// Get the current band energies (0.0-1.0 per band).
    pub fn band_energies(&self) -> Vec<f32> {
        self.energies.lock().unwrap().clone()
    }
}

/// Compute `num_bands + 1` logarithmically-spaced frequency edges
/// from `min_freq` to `max_freq`.
fn log_band_edges(num_bands: usize, min_freq: f32, max_freq: f32) -> Vec<f32> {
    let log_min = min_freq.ln();
    let log_max = max_freq.ln();
    (0..=num_bands)
        .map(|i| (log_min + (log_max - log_min) * i as f32 / num_bands as f32).exp())
        .collect()
}
