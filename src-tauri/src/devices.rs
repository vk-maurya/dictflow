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
}
