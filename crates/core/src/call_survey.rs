//! When Desktop asks "How was your call?" (`ts/util/callQualitySurvey.dom.ts`,
//! `ts/services/calling.preload.ts`): after a call RingRTC calls a survey
//! candidate, at most once a day, always (within a day of the last such) for
//! one that failed, else for a share of calls set by the remote config's
//! `desktop.callQualitySurveyPPM` (1% without it).
//!
//! The answers go with the call's statistics (Desktop's
//! `submitCallQualitySurvey` in `ts/state/ducks/calling.preload.ts`). Desktop
//! sends them through libsignal-net's `UnauthCallQualityService` since commit
//! 97a1b1ef6, a gRPC call; langis's libsignal-net-chat has the same call
//! (`Unauth::submit_call_quality_survey`) but langis opens no gRPC
//! connection, so they go as Desktop sent them before that commit: `PUT
//! /v1/call_quality_survey` on the unauthenticated chat connection, a
//! `SubmitCallQualitySurveyRequest` protobuf (Desktop's removed
//! `protos/CallQualitySurvey.proto`). That message has no field for the call
//! id hash, so it isn't sent.

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
    /// RingRTC's statistics and telemetry, sent with the answers.
    pub stats: langis_calls::CallStats,
    pub telemetry: Option<Vec<u8>>,
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

/// Desktop's removed `SubmitCallQualitySurveyRequest`
/// (`protos/CallQualitySurvey.proto` before commit 97a1b1ef6).
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct SurveyRequest {
    #[prost(bool, tag = "1")]
    pub user_satisfied: bool,
    #[prost(string, repeated, tag = "2")]
    pub call_quality_issues: Vec<String>,
    #[prost(string, optional, tag = "3")]
    pub additional_issues_description: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub debug_log_url: Option<String>,
    #[prost(int64, tag = "5")]
    pub start_timestamp: i64,
    #[prost(int64, tag = "6")]
    pub end_timestamp: i64,
    #[prost(string, tag = "7")]
    pub call_type: String,
    #[prost(bool, tag = "8")]
    pub success: bool,
    #[prost(string, tag = "9")]
    pub call_end_reason: String,
    #[prost(float, optional, tag = "10")]
    pub connection_rtt_median: Option<f32>,
    #[prost(float, optional, tag = "11")]
    pub audio_rtt_median: Option<f32>,
    #[prost(float, optional, tag = "12")]
    pub video_rtt_median: Option<f32>,
    #[prost(float, optional, tag = "13")]
    pub audio_recv_jitter_median: Option<f32>,
    #[prost(float, optional, tag = "14")]
    pub video_recv_jitter_median: Option<f32>,
    #[prost(float, optional, tag = "15")]
    pub audio_send_jitter_median: Option<f32>,
    #[prost(float, optional, tag = "16")]
    pub video_send_jitter_median: Option<f32>,
    #[prost(float, optional, tag = "17")]
    pub audio_recv_packet_loss_fraction: Option<f32>,
    #[prost(float, optional, tag = "18")]
    pub video_recv_packet_loss_fraction: Option<f32>,
    #[prost(float, optional, tag = "19")]
    pub audio_send_packet_loss_fraction: Option<f32>,
    #[prost(float, optional, tag = "20")]
    pub video_send_packet_loss_fraction: Option<f32>,
    #[prost(bytes = "vec", optional, tag = "21")]
    pub call_telemetry: Option<Vec<u8>>,
}

/// The answers as Desktop sends them: no issues when it went well, the
/// description only with the `other` issue.
pub(crate) fn request(
    survey: &CallSurvey,
    satisfied: bool,
    issues: Vec<String>,
    description: Option<String>,
    debug_log_url: Option<String>,
) -> SurveyRequest {
    let other = issues.iter().any(|i| i == "other");
    let (audio, video) = (survey.stats.audio, survey.stats.video);
    SurveyRequest {
        user_satisfied: satisfied,
        call_quality_issues: if satisfied { Vec::new() } else { issues },
        additional_issues_description: description.filter(|_| !satisfied && other),
        debug_log_url,
        start_timestamp: survey.started_at,
        end_timestamp: survey.ended_at,
        call_type: survey.call_type.to_owned(),
        success: !survey.failed,
        call_end_reason: survey.end_reason.clone(),
        connection_rtt_median: survey.stats.connection_rtt_median,
        audio_rtt_median: audio.rtt_median,
        video_rtt_median: video.rtt_median,
        audio_recv_jitter_median: audio.jitter_median_recv,
        video_recv_jitter_median: video.jitter_median_recv,
        audio_send_jitter_median: audio.jitter_median_send,
        video_send_jitter_median: video.jitter_median_send,
        audio_recv_packet_loss_fraction: audio.packet_loss_recv,
        video_recv_packet_loss_fraction: video.packet_loss_recv,
        audio_send_packet_loss_fraction: audio.packet_loss_send,
        video_send_packet_loss_fraction: video.packet_loss_send,
        call_telemetry: survey.telemetry.clone(),
    }
}

/// Sends a survey on the unauthenticated chat connection (Desktop's
/// `submitCallQualitySurvey` in `ts/textsecure/WebAPI.preload.ts` before
/// commit 97a1b1ef6).
pub(crate) async fn submit(
    ws: &mut presage::libsignal_service::websocket::SignalWebSocket<presage::libsignal_service::websocket::Unidentified>,
    request: &SurveyRequest,
) -> anyhow::Result<()> {
    use prost::Message as _;
    let message = presage::proto::WebSocketRequestMessage {
        verb: Some("PUT".into()),
        path: Some("/v1/call_quality_survey".into()),
        body: Some(request.encode_to_vec()),
        headers: vec!["content-type:application/octet-stream".into()],
        id: None,
    };
    let response = tokio::time::timeout(std::time::Duration::from_secs(30), ws.request(message))
        .await
        .map_err(|_| anyhow::anyhow!("timed out"))??;
    let status = response.status.unwrap_or_default();
    anyhow::ensure!((200..300).contains(&status), "the server answered {status}");
    Ok(())
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
        langis_calls::Summary { survey_candidate: candidate, end_reason: reason.into(), started_at: 0, ended_at: 1, ..Default::default() }
    }

    #[test]
    fn requests_carry_the_answers_and_stats_as_desktop_sends_them() {
        use prost::Message as _;
        let mut stats = langis_calls::CallStats { connection_rtt_median: Some(12.5), ..Default::default() };
        stats.audio.packet_loss_recv = Some(0.25);
        let survey = CallSurvey {
            stats,
            telemetry: Some(vec![1, 2]),
            conversation: crate::ConversationId("d:x".into()),
            call_type: "group",
            started_at: 100,
            ended_at: 200,
            failed: true,
            end_reason: "ConnectionFailure".into(),
        };
        let unhappy = request(&survey, false, vec!["audio".into(), "other".into()], Some("robot".into()), None);
        assert_eq!(unhappy.call_quality_issues, ["audio", "other"]);
        assert_eq!(unhappy.additional_issues_description.as_deref(), Some("robot"));
        assert!(!unhappy.success);
        assert_eq!((unhappy.start_timestamp, unhappy.end_timestamp, unhappy.call_type.as_str()), (100, 200, "group"));
        assert_eq!((unhappy.connection_rtt_median, unhappy.audio_recv_packet_loss_fraction), (Some(12.5), Some(0.25)));
        assert_eq!(unhappy.call_telemetry.as_deref(), Some(&[1u8, 2][..]));
        // Round trips through the wire format.
        assert_eq!(SurveyRequest::decode(unhappy.encode_to_vec().as_slice()).unwrap(), unhappy);
        // Satisfied: no issues, no description.
        let happy = request(&survey, true, vec!["audio".into()], Some("x".into()), None);
        assert!(happy.call_quality_issues.is_empty() && happy.additional_issues_description.is_none());
        // A description without "other" isn't sent.
        assert_eq!(request(&survey, false, vec!["video".into()], Some("x".into()), None).additional_issues_description, None);
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
