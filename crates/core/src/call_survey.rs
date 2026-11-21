//! When Desktop asks "How was your call?" (`ts/util/callQualitySurvey.dom.ts`,
//! `ts/services/calling.preload.ts`): after a call RingRTC calls a survey
//! candidate, at most once a day, always (within a day of the last such) for
//! one that failed, else for a share of calls set by the remote config's
//! `desktop.callQualitySurveyPPM` (1% without it). Desktop sends the answers
//! to Signal's gRPC call quality service, which langis can't reach: the
//! survey is the frontend's alone.

/// Settings key: when the survey was last offered (unix millis; Desktop's
/// `lastCallQualitySurveyTime`).
pub const SETTING_LAST_SURVEY: &str = "call_quality_survey_at";
/// Settings key: when it was last offered for a failed call (Desktop's
/// `lastCallQualityFailureSurveyTime`).
pub const SETTING_LAST_FAILURE_SURVEY: &str = "call_quality_failure_survey_at";
/// The remote config key with the share of calls asked, per country.
pub const REMOTE_PPM: &str = "desktop.callQualitySurveyPPM";
/// Desktop's `DEFAULT_PPM`: 1%.
pub const DEFAULT_PPM: u64 = 10_000;
/// Desktop's `SURVEY_COOLDOWN`.
const COOLDOWN: i64 = 24 * 60 * 60 * 1000;

/// A call to ask about, as `Event::CallQualitySurvey`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSurvey {
    pub conversation: crate::ConversationId,
    /// Desktop's `CallQualitySurvey.CallType`: `direct_voice`,
    /// `direct_video`, `group` or `call_link`.
    pub call_type: &'static str,
    /// When it connected and ended (unix millis).
    pub started_at: i64,
    pub ended_at: i64,
    /// It ended on a failure (Desktop's `isCallFailure`).
    pub failed: bool,
    /// RingRTC's end reason, as text.
    pub end_reason: String,
}

/// Desktop's `shouldShowCallQualitySurvey`; `roll` is uniform in [0, 1).
pub fn should_show(summary: &langis_calls::Summary, last: Option<i64>, last_failure: Option<i64>, now: i64, ppm: u64, roll: f64) -> bool {
    if !summary.survey_candidate {
        return false;
    }
    if summary.failed() && last_failure.is_none_or(|at| now - at > COOLDOWN) {
        return true;
    }
    if last.is_some_and(|at| now - at <= COOLDOWN) {
        return false;
    }
    roll < ppm as f64 / 1_000_000.
}

/// The share asked for our number's country (`getCallQualitySurveyPPM`).
pub fn ppm(config: Option<&str>, e164: Option<&str>) -> u64 {
    config.filter(|c| !c.is_empty()).and_then(|c| crate::release_notes::country_share(c, e164)).unwrap_or(DEFAULT_PPM)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(candidate: bool, reason: &str) -> langis_calls::Summary {
        langis_calls::Summary { survey_candidate: candidate, end_reason: reason.into(), started_at: 0, ended_at: 1 }
    }

    #[test]
    fn asks_as_desktop_does() {
        let now = 10 * COOLDOWN;
        let ok = summary(true, "LocalHangup");
        let failed = summary(true, "ConnectionFailure");
        assert!(!should_show(&summary(false, "ConnectionFailure"), None, None, now, 1_000_000, 0.));
        // A failure, unless one was asked about within a day.
        assert!(should_show(&failed, Some(now - 1), None, now, 0, 0.99));
        assert!(!should_show(&failed, Some(now - 1), Some(now - 1), now, 0, 0.99));
        // Otherwise a share of calls, at most daily.
        assert!(should_show(&ok, None, None, now, 10_000, 0.009));
        assert!(!should_show(&ok, None, None, now, 10_000, 0.011));
        assert!(!should_show(&ok, Some(now - COOLDOWN + 1), None, now, 1_000_000, 0.));
        assert!(should_show(&ok, Some(now - COOLDOWN - 1), None, now, 1_000_000, 0.5));
    }

    #[test]
    fn the_share_follows_the_country() {
        assert_eq!(ppm(None, Some("+15550100")), DEFAULT_PPM);
        assert_eq!(ppm(Some("1:500,*:20"), Some("+15550100")), 500);
        assert_eq!(ppm(Some("1:500,*:20"), Some("+445550100")), 20);
        assert_eq!(ppm(Some("44:500"), Some("+15550100")), DEFAULT_PPM);
    }
}
