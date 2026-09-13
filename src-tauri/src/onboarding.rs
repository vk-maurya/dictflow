//! First-run wizard steps. Skip is allowed at every step (UI sets `onboarded`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnboardStep {
    Welcome,
    TalkKey,
    MicTest,
    Download,
    Done,
}

impl OnboardStep {
    pub fn as_str(self) -> &'static str {
        match self {
            OnboardStep::Welcome => "welcome",
            OnboardStep::TalkKey => "talkkey",
            OnboardStep::MicTest => "mictest",
            OnboardStep::Download => "download",
            OnboardStep::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "welcome" => Some(OnboardStep::Welcome),
            "talkkey" => Some(OnboardStep::TalkKey),
            "mictest" => Some(OnboardStep::MicTest),
            "download" => Some(OnboardStep::Download),
            "done" => Some(OnboardStep::Done),
            _ => None,
        }
    }
}

pub fn next(step: OnboardStep) -> OnboardStep {
    match step {
        OnboardStep::Welcome => OnboardStep::TalkKey,
        OnboardStep::TalkKey => OnboardStep::MicTest,
        OnboardStep::MicTest => OnboardStep::Download,
        OnboardStep::Download | OnboardStep::Done => OnboardStep::Done,
    }
}

pub fn prev(step: OnboardStep) -> OnboardStep {
    match step {
        OnboardStep::Welcome => OnboardStep::Welcome,
        OnboardStep::TalkKey => OnboardStep::Welcome,
        OnboardStep::MicTest => OnboardStep::TalkKey,
        OnboardStep::Download => OnboardStep::MicTest,
        OnboardStep::Done => OnboardStep::Download,
    }
}

/// Recommended model for the download step. P4 may replace this with RAM scoring.
pub fn recommended_model_id() -> &'static str {
    "parakeet-v3"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_next_prev() {
        let mut s = OnboardStep::Welcome;
        s = next(s);
        assert_eq!(s, OnboardStep::TalkKey);
        s = next(s);
        assert_eq!(s, OnboardStep::MicTest);
        s = next(s);
        assert_eq!(s, OnboardStep::Download);
        s = next(s);
        assert_eq!(s, OnboardStep::Done);
        assert_eq!(next(OnboardStep::Done), OnboardStep::Done);
        assert_eq!(prev(OnboardStep::Welcome), OnboardStep::Welcome);
        assert_eq!(prev(OnboardStep::Download), OnboardStep::MicTest);
        assert_eq!(prev(OnboardStep::Done), OnboardStep::Download);
    }

    #[test]
    fn recommended_is_parakeet_v3() {
        assert_eq!(recommended_model_id(), "parakeet-v3");
    }

    #[test]
    fn step_round_trip() {
        assert_eq!(OnboardStep::parse("talkkey"), Some(OnboardStep::TalkKey));
        assert_eq!(OnboardStep::Download.as_str(), "download");
        assert_eq!(OnboardStep::parse("nope"), None);
    }
}
