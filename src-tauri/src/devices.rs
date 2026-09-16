//! Device pick + live level math. No cpal — `main.rs` lists names and opens
//! the chosen endpoint.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceChoice {
    pub name: String,
    pub fell_back: bool,
}

/// Prefer `preferred` when it is still plugged in; otherwise the OS default.
pub fn choose_device(
    names: &[String],
    default_name: Option<&str>,
    preferred: Option<&str>,
) -> Result<DeviceChoice, String> {
    if names.is_empty() {
        return Err("no input devices found".to_owned());
    }
    if let Some(want) = preferred.map(str::trim).filter(|s| !s.is_empty()) {
        if names.iter().any(|n| n == want) {
            return Ok(DeviceChoice {
                name: want.to_owned(),
                fell_back: false,
            });
        }
        let fallback = default_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| names.first().map(|s| s.as_str()))
            .ok_or_else(|| "no input devices found".to_owned())?;
        return Ok(DeviceChoice {
            name: fallback.to_owned(),
            fell_back: true,
        });
    }
    let name = default_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| names.first().map(|s| s.as_str()))
        .ok_or_else(|| "no default input device".to_owned())?;
    Ok(DeviceChoice {
        name: name.to_owned(),
        fell_back: false,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioLevel {
    pub peak: f32,
    pub rms: f32,
}

pub fn level_from_samples(samples: &[f32]) -> AudioLevel {
    if samples.is_empty() {
        return AudioLevel { peak: 0.0, rms: 0.0 };
    }
    let mut peak = 0.0f32;
    let mut sum = 0.0f32;
    for s in samples {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sum += s * s;
    }
    AudioLevel {
        peak,
        rms: (sum / samples.len() as f32).sqrt(),
    }
}

/// Vertical-bar count for the SpeakType-style live waveform.
pub const WAVE_BINS: usize = 40;

/// Peak per equal slice of `samples`, for the floating recording HUD.
pub fn peak_bins(samples: &[f32], n: usize) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    if samples.is_empty() {
        return vec![0.0; n];
    }
    let mut out = vec![0.0f32; n];
    let len = samples.len();
    for (i, slot) in out.iter_mut().enumerate() {
        let start = i * len / n;
        let end = ((i + 1) * len / n).max(start + 1).min(len);
        let mut peak = 0.0f32;
        for s in &samples[start..end] {
            peak = peak.max(s.abs());
        }
        *slot = peak;
    }
    out
}

/// Shorter than this is a tap / empty buffer, not speech.
pub const MIN_SPEECH_SECS: f64 = 0.25;
/// Peak below this is treated as silence (Setup uses 0.02 as "working").
pub const SILENCE_PEAK: f32 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlankAudio {
    Empty,
    TooShort,
    Silence,
}

impl BlankAudio {
    pub fn message(self) -> &'static str {
        match self {
            Self::Empty => "captured no audio — check the microphone",
            Self::TooShort => "clip too short to transcribe — hold the talk key and speak",
            Self::Silence => "only silence — nothing to transcribe",
        }
    }
}

/// Reject empty, sub-quarter-second, or silent clips before local or cloud STT.
pub fn blank_audio(samples: &[f32], sample_rate: u32) -> Option<BlankAudio> {
    if samples.is_empty() || sample_rate == 0 {
        return Some(BlankAudio::Empty);
    }
    let secs = samples.len() as f64 / f64::from(sample_rate);
    if secs < MIN_SPEECH_SECS {
        return Some(BlankAudio::TooShort);
    }
    if level_from_samples(samples).peak < SILENCE_PEAK {
        return Some(BlankAudio::Silence);
    }
    None
}

pub fn is_blank_audio_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("nothing to transcribe")
        || m.contains("too short to transcribe")
        || m.contains("captured no audio")
        || m.contains("only silence")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["Headset".into(), "Built-in".into()]
    }

    #[test]
    fn preferred_present() {
        let c = choose_device(&names(), Some("Built-in"), Some("Headset")).unwrap();
        assert_eq!(c.name, "Headset");
        assert!(!c.fell_back);
    }

    #[test]
    fn preferred_missing_falls_back() {
        let c = choose_device(&names(), Some("Built-in"), Some("USB gone")).unwrap();
        assert_eq!(c.name, "Built-in");
        assert!(c.fell_back);
    }

    #[test]
    fn none_preferred_uses_default() {
        let c = choose_device(&names(), Some("Built-in"), None).unwrap();
        assert_eq!(c.name, "Built-in");
        assert!(!c.fell_back);
    }

    #[test]
    fn empty_list_errors() {
        assert!(choose_device(&[], None, Some("x")).is_err());
    }

    #[test]
    fn empty_preferred_treated_as_default() {
        let c = choose_device(&names(), Some("Built-in"), Some("  ")).unwrap();
        assert_eq!(c.name, "Built-in");
        assert!(!c.fell_back);
    }

    #[test]
    fn levels() {
        let z = level_from_samples(&[]);
        assert_eq!(z.peak, 0.0);
        assert_eq!(z.rms, 0.0);
        let silent = level_from_samples(&[0.0, 0.0]);
        assert_eq!(silent.peak, 0.0);
        let loud = level_from_samples(&[0.5, -1.0]);
        assert!((loud.peak - 1.0).abs() < f32::EPSILON);
        assert!(loud.rms > 0.7 && loud.rms < 0.8);
    }

    #[test]
    fn peak_bins_split_and_empty() {
        assert_eq!(peak_bins(&[], 4), vec![0.0, 0.0, 0.0, 0.0]);
        assert!(peak_bins(&[0.1], 0).is_empty());
        let bins = peak_bins(&[0.1, 0.2, 0.8, 0.3], 2);
        assert_eq!(bins.len(), 2);
        assert!((bins[0] - 0.2).abs() < f32::EPSILON);
        assert!((bins[1] - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn blank_audio_rejects_empty_short_and_silent() {
        assert_eq!(blank_audio(&[], 16_000), Some(BlankAudio::Empty));
        assert_eq!(blank_audio(&[0.2; 800], 16_000), Some(BlankAudio::TooShort));
        assert_eq!(blank_audio(&[0.0; 16_000], 16_000), Some(BlankAudio::Silence));
        assert_eq!(blank_audio(&[0.2; 16_000], 16_000), None);
        assert!(is_blank_audio_error(BlankAudio::Silence.message()));
    }
}
