use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use super::CaptureEngine;

const RESIDENT_STALL_TIMEOUT: Duration = Duration::from_secs(5);
static PENDING_STOPS: AtomicUsize = AtomicUsize::new(0);

// Keep automatic recovery from accumulating native sessions/cleanup workers
// when a previous native stop itself is stuck. Dropping the closure after a
// thread-spawn failure also releases this guard.
pub(super) struct PendingStop;

impl PendingStop {
    pub(super) fn new() -> Self {
        PENDING_STOPS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for PendingStop {
    fn drop(&mut self) {
        PENDING_STOPS.fetch_sub(1, Ordering::AcqRel);
    }
}

impl CaptureEngine {
    pub fn resident_retry_required(engine: Option<&Self>) -> bool {
        let Some(engine) = engine else {
            return true;
        };
        if engine.is_finished() {
            return true;
        }
        // The resident caller already gates sharing evidence, active captures,
        // startup and retry cadence. Retire a stalled session before returning
        // true, so all retained handles reject output during its replacement.
        engine.retire_stalled_remote(Instant::now(), PENDING_STOPS.load(Ordering::Acquire))
    }

    fn retire_stalled_remote(&self, now: Instant, pending_stops: usize) -> bool {
        let shared = &self.inner.shared;
        if pending_stops != 0
            || shared.source.is_local_monitor()
            || shared.has_cached_frame.load(Ordering::Acquire)
            || shared.preflight_in_progress.load(Ordering::Acquire)
            || shared.capture_requested.load(Ordering::Acquire)
        {
            return false;
        }
        // Never make the UI tick wait for a crop or an output operation.
        let Ok(state) = shared.state.try_lock() else {
            return false;
        };
        let Some(last_observed_at) = state.last_observed_at else {
            return false;
        };
        let idle = now.saturating_duration_since(last_observed_at);
        if state.latest.is_some()
            || state.last_error.is_none()
            || idle < RESIDENT_STALL_TIMEOUT
        {
            return false;
        }
        // Cache loss alone is not a failed WGC session. observe_frame runs for
        // every arrival, including frames that skip caching or fail UIA. Only
        // the combination of a lost cache, a recorded failure and prolonged
        // arrival silence permits retirement. Healthy static pixels are kept.
        shared.stopped.store(true, Ordering::Release);
        crate::diagnostics::log(format_args!(
            "capture_session_retired reason=remote_frame_stream_stalled idle_ms={} observed={} last_error_sequence={} error={:?}",
            idle.as_millis(),
            shared.observed_sequence.load(Ordering::Acquire),
            state.last_error_sequence,
            state.last_error
        ));
        drop(state);
        self.stop();
        // A new engine must obtain new authoritative UIA evidence and pixels;
        // this path neither restores a cached crop nor writes the clipboard.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{
        LocalMonitorCaptureTarget, ScreenRect,
        engine::{CaptureSource, tests::cached_remote_engine},
    };
    use std::sync::Arc;

    fn failed_remote_engine(last_observed_at: Instant) -> CaptureEngine {
        let engine = cached_remote_engine(0);
        let shared = &engine.inner.shared;
        {
            let mut state = shared.state.lock().unwrap();
            state.latest = None;
            state.content_rect = None;
            state.last_error = Some("authoritative UIA timed out".into());
            state.last_error_sequence = 1;
            state.last_observed_at = Some(last_observed_at);
        }
        shared.has_cached_frame.store(false, Ordering::Release);
        engine
    }

    #[test]
    fn lost_cache_and_silent_stream_are_retired_at_the_stall_boundary() {
        let last = Instant::now();
        let engine = failed_remote_engine(last);
        let retained = engine.clone();
        assert!(!engine.retire_stalled_remote(
            last + RESIDENT_STALL_TIMEOUT - Duration::from_millis(1),
            0
        ));
        assert!(!engine.is_finished());
        assert!(engine.retire_stalled_remote(last + RESIDENT_STALL_TIMEOUT, 0));
        assert!(retained.is_finished());
        assert!(!retained.is_ready());
        let outcome = retained.copy_latest_to_clipboard(
            &crate::capture::CaptureAuthorization::default(),
            false,
        );
        assert!(outcome.result.is_err());
        assert!(outcome.replacement.is_none());
        assert!(outcome.save_result.is_none());
    }

    #[test]
    fn a_twenty_seven_minute_stall_does_not_need_a_click_to_be_detected() {
        let now = Instant::now();
        let engine = failed_remote_engine(now - Duration::from_secs(27 * 60));
        assert!(engine.retire_stalled_remote(now, 0));
        assert!(CaptureEngine::resident_retry_required(Some(&engine)));
    }

    #[test]
    fn repeated_uia_failures_with_frame_arrivals_do_not_restart_wgc() {
        let start = Instant::now();
        let engine = failed_remote_engine(start);
        for second in 1..=60 {
            let now = start + Duration::from_secs(second);
            engine.inner.shared.observe_frame((1, 1), now).unwrap();
            assert!(!engine.retire_stalled_remote(now, 0));
        }
        assert!(!engine.is_finished());
        assert!(!engine.is_ready());
    }

    #[test]
    fn an_arrival_after_the_stall_cancels_retirement_even_before_uia_recovers() {
        let now = Instant::now();
        let engine = failed_remote_engine(now - Duration::from_secs(60));
        engine.inner.shared.observe_frame((1, 1), now).unwrap();
        assert!(!engine.retire_stalled_remote(now, 0));
        assert!(!engine.is_finished());
    }

    #[test]
    fn a_healthy_static_cached_frame_is_not_a_stalled_session() {
        let now = Instant::now();
        let engine = cached_remote_engine(0);
        engine.inner.shared.state.lock().unwrap().last_observed_at =
            Some(now - Duration::from_secs(60 * 60));
        assert!(!engine.retire_stalled_remote(now, 0));
        assert!(engine.is_ready());
    }

    #[test]
    fn active_requests_and_preflight_are_not_interrupted() {
        let now = Instant::now();
        let engine = failed_remote_engine(now - Duration::from_secs(60));
        let shared = &engine.inner.shared;
        shared.preflight_in_progress.store(true, Ordering::Release);
        assert!(!engine.retire_stalled_remote(now, 0));
        shared.preflight_in_progress.store(false, Ordering::Release);
        shared.capture_requested.store(true, Ordering::Release);
        assert!(!engine.retire_stalled_remote(now, 0));
        shared.capture_requested.store(false, Ordering::Release);
        assert!(engine.retire_stalled_remote(now, 0));
    }

    #[test]
    fn a_busy_state_lock_is_skipped_instead_of_blocking_the_resident_tick() {
        let now = Instant::now();
        let engine = failed_remote_engine(now - Duration::from_secs(60));
        let state = engine.inner.shared.state.lock().unwrap();
        assert!(!engine.retire_stalled_remote(now, 0));
        drop(state);
        assert!(engine.retire_stalled_remote(now, 0));
    }

    #[test]
    fn pending_native_cleanup_prevents_an_automatic_restart_loop() {
        let now = Instant::now();
        let engine = failed_remote_engine(now - Duration::from_secs(60));
        assert!(!engine.retire_stalled_remote(now, 1));
        assert!(!engine.is_finished());
        assert!(engine.retire_stalled_remote(now, 0));
    }

    #[test]
    fn no_recorded_failure_or_no_observation_is_not_stall_evidence() {
        let now = Instant::now();
        let engine = failed_remote_engine(now - Duration::from_secs(60));
        engine.inner.shared.state.lock().unwrap().last_error = None;
        assert!(!engine.retire_stalled_remote(now, 0));
        {
            let mut state = engine.inner.shared.state.lock().unwrap();
            state.last_error = Some("UIA unavailable".into());
            state.last_observed_at = None;
        }
        assert!(!engine.retire_stalled_remote(now, 0));
    }

    #[test]
    fn remote_stall_policy_does_not_change_local_monitor_recovery() {
        let now = Instant::now();
        let mut engine = failed_remote_engine(now - Duration::from_secs(60));
        let inner = Arc::get_mut(&mut engine.inner).unwrap();
        Arc::get_mut(&mut inner.shared).unwrap().source =
            CaptureSource::LocalMonitor(LocalMonitorCaptureTarget {
                monitor_handle: 0,
                screen_rect: ScreenRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
            });
        assert!(!engine.retire_stalled_remote(now, 0));
        assert!(!engine.is_finished());
    }
}
