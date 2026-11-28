//! Local dictation shared by desktop frontends. No microphone is opened until start.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Iced's `SttTranscriptReady` terminal delivery: raw UTF-8, with no paste
/// envelope, newline conversion, or submit appended.
pub fn terminal_transcript_bytes(text: String) -> Vec<u8> {
    text.into_bytes()
}

pub const MIN_RECORDING_SECONDS: f32 = 0.25;
pub const MIN_RMS: f32 = 0.001;
pub const MIN_PEAK: f32 = 0.01;

pub fn audio_stats(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }

    let mut sum_squares = 0.0f64;
    let mut peak = 0.0f32;
    for &sample in samples {
        let abs = sample.abs();
        peak = peak.max(abs);
        let sample = sample as f64;
        sum_squares += sample * sample;
    }

    let rms = (sum_squares / samples.len() as f64).sqrt() as f32;
    (rms, peak)
}

fn downmix_f32(data: &[f32], channels: usize, buf: &mut Vec<f32>) {
    // Convert to mono by averaging channels
    if channels == 1 {
        buf.extend_from_slice(data);
    } else {
        for chunk in data.chunks(channels) {
            let sum: f32 = chunk.iter().sum();
            buf.push(sum / channels as f32);
        }
    }
}

fn downmix_i16(data: &[i16], channels: usize, buf: &mut Vec<f32>) {
    if channels == 1 {
        buf.extend(data.iter().map(|&s| s as f32 / 32768.0));
    } else {
        for chunk in data.chunks(channels) {
            let sum: f32 = chunk.iter().map(|&s| s as f32 / 32768.0).sum();
            buf.push(sum / channels as f32);
        }
    }
}

fn resample(mono_samples: Vec<f32>, input_sample_rate: u32) -> Vec<f32> {
    let input_rate = input_sample_rate as usize;
    let output_rate = 16000usize;

    // Resample to 16kHz for Whisper using linear interpolation
    if input_rate != output_rate {
        let ratio = input_rate as f64 / output_rate as f64;
        let output_len = (mono_samples.len() as f64 / ratio) as usize;
        let mut output = Vec::with_capacity(output_len);
        for i in 0..output_len {
            let src_idx = i as f64 * ratio;
            let idx0 = src_idx as usize;
            let frac = src_idx - idx0 as f64;
            let s0 = mono_samples.get(idx0).copied().unwrap_or(0.0);
            let s1 = mono_samples.get(idx0 + 1).copied().unwrap_or(s0);
            output.push(s0 + (s1 - s0) * frac as f32);
        }
        output
    } else {
        mono_samples
    }
}

/// Whether the recording passes Iced's duration and near-silence thresholds.
pub fn should_transcribe(samples: &[f32], sample_rate: u32) -> bool {
    let duration_secs = samples.len() as f32 / sample_rate as f32;
    let (rms, peak) = audio_stats(samples);
    !(duration_secs < MIN_RECORDING_SECONDS || (rms < MIN_RMS && peak < MIN_PEAK))
}

/// Iced's stop diagnostics, including the microphone-permission hint for silence.
/// Formatting stays shared and can be checked without opening an input device.
pub fn recording_diagnostics(samples: &[f32], sample_rate: u32) -> String {
    if samples.is_empty() {
        return "[STT] Recording stopped with no audio samples".into();
    }
    let duration_secs = samples.len() as f32 / sample_rate as f32;
    let (rms, peak) = audio_stats(samples);
    let mut message =
        format!(
        "[STT] Recording stopped: samples={} sample_rate={} duration={:.2}s rms={:.6} peak={:.6}",
        samples.len(), sample_rate, duration_secs, rms, peak,
    );
    if !should_transcribe(samples, sample_rate) {
        if duration_secs < MIN_RECORDING_SECONDS {
            message.push_str(&format!(
                "\n[STT] Ignoring recording shorter than {:.2}s",
                MIN_RECORDING_SECONDS,
            ));
        } else {
            message.push_str(&format!(
                "\n[STT] Ignoring near-silent recording (rms={rms:.6}, peak={peak:.6}); check macOS microphone permission/input device",
            ));
        }
    }
    message
}

/// A running microphone stream and its mono sample buffer.
/// Drop or stop it on the thread where it was started (CPAL's platform contract).
pub struct Recorder {
    stream: cpal::Stream,
    audio_buffer: Arc<Mutex<Vec<f32>>>,
    sample_rate: u32,
}

impl Recorder {
    /// Use a supplied device/config, or Iced's default-input/fallback selection.
    pub fn start(
        input: Option<(cpal::Device, cpal::SupportedStreamConfig)>,
        audio_buffer: Arc<Mutex<Vec<f32>>>,
    ) -> Result<Self, String> {
        let (device, config) = match input {
            Some(input) => input,
            None => Self::default_input()?,
        };
        let sample_rate = config.sample_rate().0;
        let device_name = device
            .name()
            .unwrap_or_else(|_| "<unknown input device>".to_string());
        eprintln!(
            "[STT] Starting recording: device={} sample_rate={} channels={} format={:?}",
            device_name,
            sample_rate,
            config.channels(),
            config.sample_format()
        );

        {
            let mut buf = audio_buffer.lock().unwrap();
            buf.clear();
        }

        let channels = config.channels() as usize;
        let buf = audio_buffer.clone();

        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device
                .build_input_stream(
                    &config.into(),
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        let mut buf = buf.lock().unwrap();
                        downmix_f32(data, channels, &mut buf);
                    },
                    |err| eprintln!("[STT] Audio stream error: {}", err),
                    None,
                )
                .map_err(|e| format!("Failed to build input stream: {}", e))?,
            cpal::SampleFormat::I16 => device
                .build_input_stream(
                    &config.into(),
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        let mut buf = buf.lock().unwrap();
                        downmix_i16(data, channels, &mut buf);
                    },
                    |err| eprintln!("[STT] Audio stream error: {}", err),
                    None,
                )
                .map_err(|e| format!("Failed to build input stream: {}", e))?,
            format => return Err(format!("Unsupported sample format: {:?}", format)),
        };

        stream
            .play()
            .map_err(|e| format!("Failed to start stream: {}", e))?;

        Ok(Self {
            stream,
            audio_buffer,
            sample_rate,
        })
    }

    fn default_input() -> Result<(cpal::Device, cpal::SupportedStreamConfig), String> {
        let host = cpal::default_host();
        let default = host.default_input_device();
        let default_config = default.as_ref().map(|device| device.default_input_config());
        let (device, config) = match (default, default_config) {
            (Some(device), Some(Ok(config))) => (device, config),
            (_, default_err) => {
                // macOS can leave the default input pointing at a device that
                // has gone (a headset or iPhone mic that disconnected; seen
                // 2026-10-23). Record from the first input that answers instead.
                let reason = match default_err {
                    Some(Err(e)) => format!("the default input device failed ({e})"),
                    _ => "there is no default input device".to_string(),
                };
                let fallback = host
                    .input_devices()
                    .map_err(|e| format!("Failed to list input devices after {reason}: {e}"))?
                    .find_map(|device| {
                        let config = device.default_input_config().ok()?;
                        Some((device, config))
                    })
                    .ok_or_else(|| format!("No working input device: {reason}"))?;
                eprintln!(
                    "[STT] {reason}; recording from {} instead",
                    fallback
                        .0
                        .name()
                        .unwrap_or_else(|_| "<unknown input device>".to_string())
                );
                fallback
            }
        };

        Ok((device, config))
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Stop the stream before taking the captured mono samples.
    pub fn stop(self) -> Vec<f32> {
        drop(self.stream);
        let mut buf = self.audio_buffer.lock().unwrap();
        std::mem::take(&mut *buf)
    }
}

/// A loaded Whisper model. The host controls its lifetime, as Iced did before extraction.
pub struct Transcriber {
    context: whisper_rs::WhisperContext,
}

impl Transcriber {
    pub fn load(model_path: &Path) -> Result<Self, String> {
        if !model_path.exists() {
            let model_dir = model_path.parent().unwrap_or(model_path).display();
            return Err(format!(
                "Model not found. Download it with:\n  \
                 mkdir -p \"{model_dir}\" && \\\n  \
                 curl -L -o \"{}\" \\\n  \
                 https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
                model_path.display()
            ));
        }
        let path = model_path.to_str().ok_or_else(|| {
            format!(
                "Failed to load model: path is not valid UTF-8: {}",
                model_path.display()
            )
        })?;
        let context = whisper_rs::WhisperContext::new_with_params(
            path,
            whisper_rs::WhisperContextParameters::default(),
        )
        .map_err(|e| format!("Failed to load model: {}", e))?;
        Ok(Self { context })
    }

    pub fn transcribe(
        &self,
        mono_samples: Vec<f32>,
        input_sample_rate: u32,
    ) -> Result<String, String> {
        let resampled = resample(mono_samples, input_sample_rate);
        // Run Whisper
        let mut state = self
            .context
            .create_state()
            .map_err(|e| format!("Failed to create whisper state: {}", e))?;

        let mut params =
            whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some("en"));
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        params.set_no_speech_thold(0.6);

        state
            .full(params, &resampled)
            .map_err(|e| format!("Whisper transcription failed: {}", e))?;

        let num_segments = state.full_n_segments();
        let mut result = String::new();
        for i in 0..num_segments {
            if let Some(segment) = state.get_segment(i) {
                if let Ok(segment_text) = segment.to_str_lossy() {
                    result.push_str(&segment_text);
                }
            }
        }

        Ok(result.trim().to_string())
    }
}

/// Transcribe with a model loaded for this call; hosts may retain a `Transcriber` instead.
pub fn transcribe(
    samples: Vec<f32>,
    sample_rate: u32,
    model_path: &Path,
) -> Result<String, String> {
    Transcriber::load(model_path)?.transcribe(samples, sample_rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_transcript_bytes_match_iced_write() {
        let text = "  spoken café\r\nnext\n\x1b[201~  ";
        assert_eq!(terminal_transcript_bytes(text.into()), text.as_bytes());
        assert!(terminal_transcript_bytes(String::new()).is_empty());
    }
    #[test]
    fn downmix() {
        let mut out = Vec::new();
        downmix_f32(&[1.0, -1.0, 0.5, 0.25], 2, &mut out);
        assert_eq!(out, [0.0, 0.375]);
        out.clear();
        downmix_i16(&[32767, -32768, 16384, 0], 2, &mut out);
        assert_eq!(out, [-1.0 / 65536.0, 0.25]);
    }
    #[test]
    fn resampling() {
        assert_eq!(resample(vec![0.0, 1.0], 8000), [0.0, 0.5, 1.0, 1.0]);
        assert_eq!(
            resample(vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0], 48000),
            [0.0, 3.0]
        );
        assert_eq!(resample(vec![0.0, 1.0], 16000), [0.0, 1.0]);
    }
    #[test]
    fn filter() {
        assert!(!should_transcribe(&[], 16000));
        assert!(!should_transcribe(&vec![1.0; 3999], 16000));
        assert!(should_transcribe(&vec![1.0; 4000], 16000));
        assert!(!should_transcribe(&vec![0.000999; 4000], 16000));
        assert!(should_transcribe(&vec![0.001; 4000], 16000));
        let mut samples = vec![0.0; 4000];
        samples[0] = 0.01;
        assert!(should_transcribe(&samples, 16000));
        samples[0] = 0.009999;
        assert!(!should_transcribe(&samples, 16000));
    }

    #[test]
    fn mono_and_partial_frames_preserve_capture_arithmetic() {
        let mut out = Vec::new();
        downmix_f32(&[0.5, -0.25], 1, &mut out);
        assert_eq!(out, [0.5, -0.25]);
        out.clear();
        downmix_i16(&[i16::MIN, 0, i16::MAX], 1, &mut out);
        assert_eq!(out, [-1.0, 0.0, 32767.0 / 32768.0]);
        out.clear();
        downmix_f32(&[1.0], 2, &mut out);
        downmix_i16(&[16384], 2, &mut out);
        assert_eq!(out, [0.5, 0.25]);
    }

    #[test]
    fn empty_audio_and_resampling_rounding() {
        assert_eq!(audio_stats(&[]), (0.0, 0.0));
        let (rms, peak) = audio_stats(&[0.0, 0.5, -0.5]);
        assert!((rms - (0.5f32 / 3.0).sqrt()).abs() < 0.000001);
        assert_eq!(peak, 0.5);
        assert!(resample(vec![], 48000).is_empty());
        assert!(resample(vec![1.0, 2.0], 48000).is_empty());
        assert_eq!(resample(vec![0.0, 1.0, 2.0], 24000), [0.0, 1.5]);
    }

    #[test]
    fn recording_diagnostics_match_iced_without_a_microphone() {
        assert_eq!(
            recording_diagnostics(&[], 16000),
            "[STT] Recording stopped with no audio samples"
        );
        assert_eq!(recording_diagnostics(&vec![0.5; 4000], 16000),
            "[STT] Recording stopped: samples=4000 sample_rate=16000 duration=0.25s rms=0.500000 peak=0.500000");
        assert_eq!(recording_diagnostics(&vec![0.5; 1600], 16000),
            "[STT] Recording stopped: samples=1600 sample_rate=16000 duration=0.10s rms=0.500000 peak=0.500000\n[STT] Ignoring recording shorter than 0.25s");
        assert_eq!(recording_diagnostics(&vec![0.0; 4000], 16000),
            "[STT] Recording stopped: samples=4000 sample_rate=16000 duration=0.25s rms=0.000000 peak=0.000000\n[STT] Ignoring near-silent recording (rms=0.000000, peak=0.000000); check macOS microphone permission/input device");
    }

    #[test]
    fn missing_model_error_matches_iced() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("missing.bin");
        let error = transcribe(vec![0.0; 16000], 16000, &model).unwrap_err();
        assert_eq!(error, format!(
            "Model not found. Download it with:\n  mkdir -p \"{}\" && \\\n  curl -L -o \"{}\" \\\n  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
            dir.path().display(), model.display()
        ));
    }

    #[test]
    fn invalid_model_returns_error_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("invalid.bin");
        std::fs::write(&model, b"not a whisper model").unwrap();
        let error = transcribe(vec![0.0; 16000], 16000, &model).unwrap_err();
        assert!(error.starts_with("Failed to load model: "), "{error}");
    }
}
