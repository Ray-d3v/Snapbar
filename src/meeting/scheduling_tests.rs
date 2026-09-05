use super::*;

fn evidence(id: u32, leave: bool, call: bool, video: bool, shared: bool) -> MeetingEvidence {
    MeetingEvidence {
        target: CaptureTarget {
            id,
            title: "Meeting | Microsoft Teams".to_string(),
            app_name: "ms-teams.exe".to_string(),
        },
        minimized: false,
        visible: true,
        focused: true,
        has_webview: true,
        has_video: video,
        has_leave_control: leave,
        has_call_control: call,
        has_shared_content: shared,
        score: 300,
    }
}

#[test]
fn strong_evidence_requires_250ms_from_the_first_candidate() {
    let started = Instant::now();
    let candidate = evidence(42, true, true, false, false);
    let mut state = DebouncedMeetingState::default();

    assert!(state.update(started, vec![candidate.clone()]).is_none());
    assert_eq!(
        state.confirmation_deadline(),
        Some(started + ENTRY_STABLE_FOR)
    );
    assert!(
        state
            .update(
                started + Duration::from_millis(249),
                vec![candidate.clone()]
            )
            .is_none()
    );
    assert_eq!(
        state
            .update(started + ENTRY_STABLE_FOR, vec![candidate])
            .map(|meeting| meeting.target.id),
        Some(42)
    );
    assert!(state.confirmation_deadline().is_none());
}

#[test]
fn transient_evidence_loss_discards_the_entry_candidate_and_deadline() {
    let started = Instant::now();
    let candidate = evidence(42, true, true, false, false);
    let mut state = DebouncedMeetingState::default();

    assert!(state.update(started, vec![candidate.clone()]).is_none());
    assert!(
        state
            .update(started + Duration::from_millis(100), Vec::new())
            .is_none()
    );
    assert!(state.entry.is_none());
    assert!(state.confirmation_deadline().is_none());
    assert!(
        state
            .update(started + ENTRY_STABLE_FOR, vec![candidate])
            .is_none()
    );
}

#[test]
fn fallback_evidence_still_uses_the_1100ms_entry_window() {
    let started = Instant::now();
    let candidate = evidence(42, false, true, false, false);
    let mut state = DebouncedMeetingState::default();

    assert!(state.update(started, vec![candidate.clone()]).is_none());
    assert!(
        state
            .update(
                started + Duration::from_millis(1_099),
                vec![candidate.clone()]
            )
            .is_none()
    );
    assert_eq!(
        state
            .update(started + ENTRY_FALLBACK_STABLE_FOR, vec![candidate])
            .map(|meeting| meeting.target.id),
        Some(42)
    );
}

#[test]
fn replacing_the_best_candidate_restarts_its_stability_deadline() {
    let started = Instant::now();
    let first = evidence(42, true, true, false, false);
    let replacement = evidence(77, true, true, false, false);
    let mut state = DebouncedMeetingState::default();

    assert!(state.update(started, vec![first]).is_none());
    assert!(
        state
            .update(
                started + Duration::from_millis(200),
                vec![replacement.clone()]
            )
            .is_none()
    );
    assert!(
        state
            .update(
                started + Duration::from_millis(449),
                vec![replacement.clone()]
            )
            .is_none()
    );
    assert_eq!(
        state
            .update(started + Duration::from_millis(450), vec![replacement])
            .map(|meeting| meeting.target.id),
        Some(77)
    );
}

#[test]
fn scan_wake_deadline_respects_not_before_for_pending_and_scheduled_scans() {
    let now = Instant::now();
    let not_before = now + Duration::from_millis(100);
    let scheduled = now + Duration::from_millis(10);

    assert_eq!(scan_wake_deadline(true, not_before, scheduled), not_before);
    assert_eq!(scan_wake_deadline(false, not_before, scheduled), not_before);
    assert_eq!(
        scan_wake_deadline(false, not_before, now + Duration::from_millis(250)),
        now + Duration::from_millis(250)
    );
}

#[test]
fn scan_requests_coalesce_without_losing_pending_work_during_the_minimum_gap() {
    let signal = Arc::new(ScanSignal::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let now = Instant::now();
    let not_before = now + Duration::from_millis(80);
    let (completed_tx, completed_rx) = std::sync::mpsc::channel();
    let worker_signal = Arc::clone(&signal);
    let worker_stop = Arc::clone(&stop);
    let worker = thread::spawn(move || {
        worker_signal.wait(&worker_stop, not_before, now + Duration::from_secs(60));
        let _ = completed_tx.send(Instant::now());
    });

    signal.request();
    signal.request();
    signal.request();
    let completed = completed_rx.recv_timeout(Duration::from_secs(2));
    // Clean up even if a regression causes the pending request to be lost.
    {
        let _pending = signal.pending.lock().unwrap();
        stop.store(true, Ordering::Release);
        signal.wake.notify_all();
    }
    worker.join().unwrap();
    assert!(completed.expect("pending scan must wake without the watchdog") >= not_before);
    assert!(!*signal.pending.lock().unwrap());
}

#[test]
fn full_meeting_exit_retains_the_1400ms_debounce() {
    let started = Instant::now();
    let candidate = evidence(42, true, true, false, false);
    let mut state = DebouncedMeetingState::default();

    assert!(state.update(started, vec![candidate.clone()]).is_none());
    assert!(
        state
            .update(started + ENTRY_STABLE_FOR, vec![candidate])
            .is_some()
    );
    assert!(
        state
            .update(
                started + ENTRY_STABLE_FOR + Duration::from_millis(1),
                Vec::new()
            )
            .is_some()
    );
    assert!(
        state
            .update(
                started + ENTRY_STABLE_FOR + Duration::from_millis(1_399),
                Vec::new(),
            )
            .is_some()
    );
    assert!(
        state
            .update(
                started + ENTRY_STABLE_FOR + Duration::from_millis(1_401),
                Vec::new(),
            )
            .is_none()
    );
}
