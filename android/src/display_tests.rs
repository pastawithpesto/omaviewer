use super::*;
use tokio::sync::mpsc::error::TryRecvError;

fn app_with_input() -> (WindowLinkApp, tokio::sync::mpsc::Receiver<RdpInputEvent>) {
    let (input, events) = RdpInputSender::channel(16);
    let mut app = WindowLinkApp::default();
    app.started = true;
    // Model the settled Overview state: the client has already paused graphics.
    app.stream_suppressed = true;
    app.input = Some(input);
    (app, events)
}

fn assert_updates(events: &mut tokio::sync::mpsc::Receiver<RdpInputEvent>, expected: bool) {
    assert!(matches!(
        events.try_recv(),
        Ok(RdpInputEvent::SetDisplayUpdates(value)) if value == expected
    ));
}

#[test]
fn latest_frame_keeps_newest_valid_and_rejects_malformed() {
    let frames = LatestFrame::default();
    frames.replace(vec![1, 2, 3, 255], [1, 1]);
    frames.replace(vec![4, 5, 6, 255], [1, 1]);
    assert_eq!(frames.take(), Some((vec![4, 5, 6, 255], [1, 1])));

    frames.replace(vec![7, 8, 9, 255], [1, 1]);
    frames.replace(vec![0; 3], [1, 1]);
    assert_eq!(frames.take(), Some((vec![7, 8, 9, 255], [1, 1])));
    assert_eq!(frames.take(), None);
}

#[test]
fn overview_requests_initial_snapshot_then_pauses_until_interval_is_due() {
    let (mut app, mut events) = app_with_input();
    app.update_stream_visibility();
    assert_updates(&mut events, true);
    assert!(!app.stream_suppressed);
    assert!(app.snapshot_requested_at.is_some());

    app.snapshot_requested_at = None;
    app.last_snapshot = Some(Instant::now());
    app.update_stream_visibility();
    assert_updates(&mut events, false);
    assert!(app.stream_suppressed);

    app.last_snapshot = Some(Instant::now() - Duration::from_secs(31));
    app.update_stream_visibility();
    assert_updates(&mut events, true);
    assert!(!app.stream_suppressed);
}

#[test]
fn files_cancels_snapshot_without_counting_it_and_fullscreen_resumes_live_updates() {
    let (mut app, mut events) = app_with_input();
    app.last_snapshot = Some(Instant::now() - Duration::from_secs(30));
    app.update_stream_visibility();
    assert_updates(&mut events, true);
    let previous_snapshot = app.last_snapshot;
    assert!(app.snapshot_requested_at.is_some());

    app.view_mode = ViewMode::Files;
    app.update_stream_visibility();
    assert_updates(&mut events, false);
    assert_eq!(app.last_snapshot, previous_snapshot);
    assert!(app.snapshot_requested_at.is_none());

    app.view_mode = ViewMode::FullScreen;
    app.update_stream_visibility();
    assert_updates(&mut events, true);
    assert!(app.snapshot_requested_at.is_none());

    app.view_mode = ViewMode::Overview;
    app.last_snapshot = Some(Instant::now());
    app.update_stream_visibility();
    assert_updates(&mut events, false);
}

#[test]
fn snapshot_timeout_pauses_and_manual_interval_does_not_repeat() {
    let (mut app, mut events) = app_with_input();
    app.update_stream_visibility();
    assert_updates(&mut events, true);
    app.snapshot_requested_at = Some(Instant::now() - Duration::from_secs(4));
    app.update_stream_visibility();
    assert_updates(&mut events, false);
    assert!(app.stream_suppressed);
    assert!(app.last_snapshot.is_some());

    app.window_picker.set_preview_interval_secs(0);
    app.update_stream_visibility();
    assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));
    assert!(app.stream_suppressed);
}
