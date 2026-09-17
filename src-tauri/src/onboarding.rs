//! First-run wizard steps. Skip is allowed at every step (UI sets `onboarded`).
//!
//! macOS adds a `Permissions` step between Welcome and TalkKey for
//! Microphone + Accessibility (SpeakType-style). Fn uses a session event tap.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnboardStep {
    Welcome,
    /// macOS only: Microphone + Accessibility grants.
    Permissions,
    TalkKey,
    MicTest,
    Download,
    Done,
}

impl OnboardStep {
    pub fn as_str(self) -> &'static str {
        match self {
            OnboardStep::Welcome => "welcome",
            OnboardStep::Permissions => "permissions",
            OnboardStep::TalkKey => "talkkey",
            OnboardStep::MicTest => "mictest",
            OnboardStep::Download => "download",
            OnboardStep::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "welcome" => Some(OnboardStep::Welcome),
            "permissions" => Some(OnboardStep::Permissions),
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
        OnboardStep::Welcome => {
            #[cfg(target_os = "macos")]
            return OnboardStep::Permissions;
            #[cfg(not(target_os = "macos"))]
            return OnboardStep::TalkKey;
        }
        OnboardStep::Permissions => OnboardStep::TalkKey,
        OnboardStep::TalkKey => OnboardStep::MicTest,
        OnboardStep::MicTest => OnboardStep::Download,
        OnboardStep::Download | OnboardStep::Done => OnboardStep::Done,
    }
}

pub fn prev(step: OnboardStep) -> OnboardStep {
    match step {
        OnboardStep::Welcome => OnboardStep::Welcome,
        OnboardStep::Permissions => OnboardStep::Welcome,
        OnboardStep::TalkKey => {
            #[cfg(target_os = "macos")]
            return OnboardStep::Permissions;
            #[cfg(not(target_os = "macos"))]
            return OnboardStep::Welcome;
        }
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
    fn step_round_trip() {
        assert_eq!(OnboardStep::parse("talkkey"), Some(OnboardStep::TalkKey));
        assert_eq!(OnboardStep::Download.as_str(), "download");
        assert_eq!(OnboardStep::parse("nope"), None);
        assert_eq!(OnboardStep::parse("permissions"), Some(OnboardStep::Permissions));
    }

    #[test]
    fn recommended_is_parakeet_v3() {
        assert_eq!(recommended_model_id(), "parakeet-v3");
    }

    #[test]
    fn recommended_matches_catalog_default() {
        assert_eq!(recommended_model_id(), crate::models::default_model_id());
    }

    #[test]
    fn done_loops_to_done() {
        assert_eq!(next(OnboardStep::Done), OnboardStep::Done);
    }

    #[test]
    fn welcome_loops_to_welcome() {
        assert_eq!(prev(OnboardStep::Welcome), OnboardStep::Welcome);
    }
}
