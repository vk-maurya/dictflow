//! 19 / 20 minute recording policy (Wispr Flow session warning).
//!
//! Always warn at 19:00. Optional hard stop at 20:00 (`session_cap`).

pub const WARN_SECS: f64 = 19.0 * 60.0;
pub const CAP_SECS: f64 = 20.0 * 60.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTick {
    None,
    Warn,
    Cap,
}

/// Decide what the live ticker should do this interval.
pub fn session_tick(elapsed_secs: f64, already_warned: bool, cap_enabled: bool) -> SessionTick {
    if elapsed_secs >= CAP_SECS && cap_enabled {
        return SessionTick::Cap;
    }
    if elapsed_secs >= WARN_SECS && !already_warned {
        return SessionTick::Warn;
    }
    SessionTick::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_warn_is_none() {
        assert_eq!(session_tick(WARN_SECS - 1.0, false, false), SessionTick::None);
        assert_eq!(session_tick(0.0, false, true), SessionTick::None);
    }

    #[test]
    fn at_warn_fires_once() {
        assert_eq!(session_tick(WARN_SECS, false, false), SessionTick::Warn);
        assert_eq!(session_tick(WARN_SECS + 30.0, true, false), SessionTick::None);
    }

    #[test]
    fn cap_off_at_twenty_stays_none_after_warn() {
        assert_eq!(session_tick(CAP_SECS, true, false), SessionTick::None);
        assert_eq!(session_tick(21.0 * 60.0, true, false), SessionTick::None);
    }

    #[test]
    fn cap_on_at_twenty_stops() {
        assert_eq!(session_tick(CAP_SECS, true, true), SessionTick::Cap);
        assert_eq!(session_tick(CAP_SECS, false, true), SessionTick::Cap);
    }

    #[test]
    fn warn_still_fires_before_cap() {
        assert_eq!(session_tick(WARN_SECS, false, true), SessionTick::Warn);
    }
}
