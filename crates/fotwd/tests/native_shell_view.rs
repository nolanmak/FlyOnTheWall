use fotw_web::recorder::{RecordingState, RecordingStatus};
use fotwd::native_shell::RecorderView;

#[test]
fn recording_uses_the_actual_capture_clock_and_keeps_stop_available() {
    let view = RecorderView::from_status(&RecordingStatus::recording(1000, 65_000));
    assert_eq!(view.state, RecordingState::Recording);
    assert_eq!(view.title, "Recording");
    assert_eq!(view.elapsed_label.as_deref(), Some("01:05"));
    assert!(view.stop_enabled);
}

#[test]
fn preparation_never_claims_recording_or_invents_a_clock() {
    for (status, title) in [
        (RecordingStatus::preparing(false), "Preparing"),
        (RecordingStatus::starting(false), "Starting"),
    ] {
        let view = RecorderView::from_status(&status);
        assert_eq!(view.state, status.state);
        assert_eq!(view.title, title);
        assert!(view.elapsed_label.is_none());
        assert!(view.stop_enabled);
    }
}

#[test]
fn cancellation_during_preparation_stays_visible_until_acknowledged() {
    let view = RecorderView::from_status(&RecordingStatus::starting(true));
    assert_eq!(view.state, RecordingState::Starting);
    assert_eq!(view.title, "Stopping");
    assert!(!view.stop_enabled);
    assert!(view.elapsed_label.is_none());
}

#[test]
fn finishing_uses_the_frozen_backend_clock_and_never_announces_saved() {
    let status = RecordingStatus::finishing(1000, 5000);
    let view = RecorderView::from_status(&status);
    assert_eq!(view.title, "Finishing");
    assert_eq!(view.state, RecordingState::Finishing);
    assert_eq!(view.elapsed_label.as_deref(), Some("00:04"));
    assert!(!view.stop_enabled);
    assert_eq!(RecorderView::from_status(&status), view);
    let idle = RecorderView::from_status(&RecordingStatus::idle());
    assert_eq!(idle.title, "Not recording");
    assert!(idle.elapsed_label.is_none());
    assert!(!idle.stop_enabled);
}

#[test]
fn a_transcription_failure_does_not_hide_continuing_capture_or_stop() {
    let status = RecordingStatus::recording(1000, 10000)
        .with_transcription_error(Some("Local speech unavailable; audio retained".into()));
    let view = RecorderView::from_status(&status);
    assert_eq!(view.state, RecordingState::Recording);
    assert!(view.stop_enabled);
    assert_eq!(view.elapsed_label.as_deref(), Some("00:10"));
    assert_eq!(view.error, status.transcription_error);
}

fn speech(
    source: fotw_web::recorder::CaptureSource,
    provider: &str,
    model: &str,
    state: fotw_web::recorder::SpeechState,
) -> fotw_web::recorder::SpeechSourceStatus {
    fotw_web::recorder::SpeechSourceStatus {
        source,
        provider: provider.into(),
        model: model.into(),
        revision: None,
        state,
        epoch: Some(2),
        from_ms: Some(100),
        switch_reason: Some("network_error".into()),
        pending_audio_ms: Some(2500),
        word_timestamps_supported: Some(false),
        diarization_supported: Some(false),
        worker_running: Some(true),
        error: None,
    }
}

#[test]
fn native_details_use_per_source_health_and_the_actual_active_engines() {
    use fotw_web::recorder::{
        CaptureDelivery, CaptureHealth, CaptureSource, SourceCaptureStatus, SpeechState,
    };
    let healthy = SourceCaptureStatus {
        state: CaptureDelivery::Healthy,
        attempts: 0,
        max_attempts: 3,
        retry_in_ms: None,
    };
    let recovering = SourceCaptureStatus {
        state: CaptureDelivery::Recovering,
        attempts: 1,
        max_attempts: 3,
        retry_in_ms: Some(1000),
    };
    let status = RecordingStatus::recording(1000, 10000)
        .with_capture_health(Some(CaptureHealth {
            system: healthy,
            mic: recovering,
        }))
        .with_speech_status(vec![
            speech(
                CaptureSource::System,
                "deepgram",
                "nova-3",
                SpeechState::Cloud,
            ),
            speech(
                CaptureSource::Mic,
                "whisper.cpp",
                "small.en",
                SpeechState::Local,
            ),
        ]);
    let view = RecorderView::from_status(&status);
    assert_eq!(view.title, "Recording");
    assert!(view.stop_enabled);
    assert!(view.detail.contains("Microphone: recovering"));
    assert!(view.detail.contains("System: deepgram (nova-3)"));
    assert!(view.detail.contains("Mic: whisper.cpp (small.en)"));
    assert!(view.detail.contains("3 s pending"));
}

#[test]
fn a_per_source_failure_is_actionable_without_an_aggregate_error() {
    use fotw_web::recorder::{CaptureSource, SpeechState};
    let mut source = speech(
        CaptureSource::Mic,
        "whisper.cpp",
        "small.en",
        SpeechState::Incomplete,
    );
    source.error = Some("Local model unavailable; retry after setup".into());
    let status = RecordingStatus::recording(1000, 10000).with_speech_status(vec![source]);
    let view = RecorderView::from_status(&status);
    assert_eq!(view.state, RecordingState::Recording);
    assert!(view.stop_enabled);
    assert!(
        view.error
            .as_deref()
            .is_some_and(|error| error.contains("Mic: Local model unavailable"))
    );
}

#[test]
fn a_missing_backend_clock_stays_unknown_while_capture_remains_visible() {
    let mut status = RecordingStatus::recording(1000, 10000);
    status.elapsed_ms = None;
    let view = RecorderView::from_status(&status);
    assert_eq!(view.title, "Recording");
    assert!(view.elapsed_label.is_none());
    assert!(view.stop_enabled);
}
