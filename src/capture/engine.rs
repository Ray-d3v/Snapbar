use std::fmt;
use std::{
    ffi::c_void,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use windows::Win32::{
    Foundation::HWND,
    System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency},
    UI::WindowsAndMessaging::{IsIconic, IsWindow},
};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::Frame,
    graphics_capture_api::InternalCaptureControl,
    monitor::Monitor as CaptureMonitor,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window as CaptureWindow,
};

use super::{
    CaptureAuthorization, CaptureReceipt, LocalMonitorCaptureTarget, ScreenRect,
    content_detector::PixelRect,
    flash::current_screen_rect,
    layout::{LayoutResolver, RemoteLayout},
    local_share::validate_local_monitor_target,
    output_capture,
};
use crate::shutdown::defer_cleanup;

const BACKUP_CACHE_INTERVAL: Duration = Duration::from_millis(750);
const FRESH_FRAME_WAIT: Duration = Duration::from_millis(200);
const LOCAL_FRESH_FRAME_WAIT: Duration = Duration::from_millis(200);
const DETECTION_RETRY_INTERVAL: Duration = Duration::from_millis(750);
const READY_TIMEOUT: Duration = Duration::from_millis(1_200);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    RemoteTeamsWindow(u32),
    LocalMonitor(LocalMonitorCaptureTarget),
}

impl CaptureSource {
    fn remote_target_id(&self) -> Option<u32> {
        match self {
            Self::RemoteTeamsWindow(target_id) => Some(*target_id),
            Self::LocalMonitor(_) => None,
        }
    }

    fn is_local_monitor(&self) -> bool {
        matches!(self, Self::LocalMonitor(_))
    }

    fn validate_remote_target(&self) -> Result<()> {
        let Some(target_id) = self.remote_target_id() else {
            return Ok(());
        };
        let hwnd = HWND(target_id as usize as *mut c_void);
        if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
            return Err(anyhow!("撮影対象のTeamsウィンドウが見つかりません"));
        }
        if unsafe { IsIconic(hwnd).as_bool() } {
            return Err(anyhow!(
                "Teamsが最小化されています。ウィンドウを復元してから撮影してください"
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct CaptureEngine {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    shared: Arc<SharedState>,
    control: Mutex<Option<CaptureControl<FrameHandler, String>>>,
}

impl Drop for EngineInner {
    fn drop(&mut self) {
        self.stop();
    }
}

impl EngineInner {
    fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.shared.has_cached_frame.store(false, Ordering::Release);
        self.shared
            .capture_requested
            .store(false, Ordering::Release);
        self.shared.ready.notify_all();
        let control = self
            .control
            .lock()
            .ok()
            .and_then(|mut control| control.take());
        if let Some(control) = control {
            control.halt_handle().store(true, Ordering::Release);
            defer_cleanup("snapbar-capture-stop", move || {
                let _ = control.stop();
            });
        }
    }
}

pub(super) struct SharedState {
    source: CaptureSource,
    layout: Option<LayoutResolver>,
    stopped: AtomicBool,
    preflight_in_progress: AtomicBool,
    capture_requested: AtomicBool,
    observed_sequence: std::sync::atomic::AtomicU64,
    has_cached_frame: AtomicBool,
    state: Mutex<RuntimeState>,
    ready: Condvar,
}

impl SharedState {
    fn observe_frame(&self, source_size: (u32, u32), now: Instant) -> Result<u64> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を更新できませんでした"))?;
        state.source_size = Some(source_size);
        state.last_frame_interval = state
            .last_observed_at
            .map(|last| now.saturating_duration_since(last));
        state.last_observed_at = Some(now);
        Ok(self
            .observed_sequence
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1))
    }

    fn backup_paused(&self, requested: bool) -> bool {
        !requested && self.preflight_in_progress.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct FrameUnavailable(&'static str);

impl fmt::Display for FrameUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for FrameUnavailable {}

pub struct CaptureOutcome {
    pub result: Result<CaptureReceipt>,
    pub replacement: Option<CaptureEngine>,
    pub save_result: Option<Result<PathBuf>>,
}

#[derive(Default)]
struct RuntimeState {
    latest: Option<CachedFrame>,
    last_observed_at: Option<Instant>,
    last_frame_interval: Option<Duration>,
    source_size: Option<(u32, u32)>,
    confirmed_layout: Option<RemoteLayout>,
    content_rect: Option<PixelRect>,
    last_error: Option<String>,
    last_error_sequence: u64,
    requested_after_sequence: u64,
}

impl RuntimeState {
    fn source_dimensions(&self) -> Option<(u32, u32)> {
        // Dimensions are not crop authorization. UIA is still read afresh at
        // preflight, cropping, and output, even after the pixel cache was lost.
        self.source_size.or_else(|| {
            self.latest
                .as_ref()
                .map(|frame| (frame.source_width, frame.source_height))
        })
    }
}

struct CachedFrame {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
    captured_at: Instant,
    content_rect: PixelRect,
    fallback_screen_rect: ScreenRect,
    source_width: u32,
    source_height: u32,
    sequence: u64,
    remote_layout: Option<RemoteLayout>,
    rendered_at_100ns: i64,
}

struct CaptureRequestGuard {
    shared: Arc<SharedState>,
}

#[derive(Clone, Copy)]
struct ArmedRequest {
    baseline_sequence: u64,
    requested_at_100ns: i64,
}

struct PreflightGuard(Arc<SharedState>);

impl PreflightGuard {
    fn new(shared: Arc<SharedState>) -> Self {
        shared.preflight_in_progress.store(true, Ordering::Release);
        Self(shared)
    }
}

impl Drop for PreflightGuard {
    fn drop(&mut self) {
        self.0.preflight_in_progress.store(false, Ordering::Release);
    }
}

impl CaptureRequestGuard {
    fn new(shared: Arc<SharedState>) -> Self {
        Self { shared }
    }
}

impl Drop for CaptureRequestGuard {
    fn drop(&mut self) {
        self.shared
            .capture_requested
            .store(false, Ordering::Release);
    }
}

pub(super) struct FrameHandler {
    shared: Arc<SharedState>,
    last_cache_update: Option<Instant>,
    last_detection: Option<Instant>,
    last_source_size: Option<(u32, u32)>,
}

impl CaptureEngine {
    pub fn start_authorized(
        source: CaptureSource,
        authorization: &CaptureAuthorization,
    ) -> Result<Self> {
        Self::start_authorized_with(authorization, || Self::start_source(source))
    }

    fn start_authorized_with(
        authorization: &CaptureAuthorization,
        start: impl FnOnce() -> Result<Self>,
    ) -> Result<Self> {
        authorization.with_current(|| Ok(()))?;
        // WGC startup waits for its native worker. Never hold the output
        // authorization mutex here: revocation runs on the UI thread.
        let engine = start()?;
        if let Err(error) = authorization.with_current(|| Ok(())) {
            engine.stop();
            return Err(error);
        }
        Ok(engine)
    }

    pub fn start_source(source: CaptureSource) -> Result<Self> {
        Self::start_source_with_layout(source, None, None)
    }

    fn start_source_with_layout(
        source: CaptureSource,
        confirmed_layout: Option<RemoteLayout>,
        resolver: Option<LayoutResolver>,
    ) -> Result<Self> {
        let layout = match resolver {
            Some(resolver) => Some(resolver),
            None => source
                .remote_target_id()
                .map(LayoutResolver::start)
                .transpose()?,
        };
        let shared = Arc::new(SharedState {
            layout,
            source: source.clone(),
            stopped: AtomicBool::new(false),
            preflight_in_progress: AtomicBool::new(false),
            capture_requested: AtomicBool::new(false),
            observed_sequence: std::sync::atomic::AtomicU64::new(0),
            has_cached_frame: AtomicBool::new(false),
            state: Mutex::new(RuntimeState {
                confirmed_layout,
                ..Default::default()
            }),
            ready: Condvar::new(),
        });
        let control = match source {
            CaptureSource::RemoteTeamsWindow(target_id) => {
                let target = CaptureWindow::from_raw_hwnd(target_id as usize as *mut c_void);
                let settings = Settings::new(
                    target,
                    CursorCaptureSettings::WithoutCursor,
                    DrawBorderSettings::WithoutBorder,
                    SecondaryWindowSettings::Default,
                    MinimumUpdateIntervalSettings::Default,
                    DirtyRegionSettings::Default,
                    ColorFormat::Rgba8,
                    Arc::clone(&shared),
                );
                FrameHandler::start_free_threaded(settings).map_err(|error| {
                    anyhow!("Teams共有画面のキャプチャを開始できませんでした: {error}")
                })?
            }
            CaptureSource::LocalMonitor(target) => {
                if !validate_local_monitor_target(&target)? {
                    return Err(anyhow!(
                        "Teamsが共有しているモニターを再確認できませんでした"
                    ));
                }
                let monitor = CaptureMonitor::from_raw_hmonitor(
                    target.monitor_handle as usize as *mut c_void,
                );
                let settings = Settings::new(
                    monitor,
                    CursorCaptureSettings::WithoutCursor,
                    DrawBorderSettings::WithoutBorder,
                    SecondaryWindowSettings::Default,
                    MinimumUpdateIntervalSettings::Default,
                    DirtyRegionSettings::Default,
                    ColorFormat::Rgba8,
                    Arc::clone(&shared),
                );
                FrameHandler::start_free_threaded(settings).map_err(|error| {
                    anyhow!("自分のTeams共有画面のキャプチャを開始できませんでした: {error}")
                })?
            }
        };

        Ok(Self {
            inner: Arc::new(EngineInner {
                shared,
                control: Mutex::new(Some(control)),
            }),
        })
    }

    pub fn is_ready(&self) -> bool {
        self.inner.shared.has_cached_frame.load(Ordering::Acquire) && !self.is_finished()
    }

    pub fn shares_session_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn diagnostic_status(&self) -> String {
        let shared = &self.inner.shared;
        let Ok(state) = shared.state.try_lock() else {
            return "state_busy".into();
        };
        format!(
            "local={} stopped={} cached={} size={:?} crop={:?} error={:?}",
            shared.source.is_local_monitor(),
            shared.stopped.load(Ordering::Acquire),
            shared.has_cached_frame.load(Ordering::Acquire),
            state.source_size,
            state.content_rect,
            state.last_error
        )
    }

    pub fn is_finished(&self) -> bool {
        self.inner.shared.stopped.load(Ordering::Acquire)
            || self.inner.control.lock().map_or(true, |control| {
                control.as_ref().is_some_and(CaptureControl::is_finished)
            })
    }

    pub fn is_local_monitor(&self) -> bool {
        self.inner.shared.source.is_local_monitor()
    }

    pub fn stop(&self) {
        self.inner.stop();
    }

    pub fn copy_latest_to_clipboard(
        &self,
        authorization: &CaptureAuthorization,
        save_to_screenshots: bool,
    ) -> CaptureOutcome {
        let request_started = Instant::now();
        crate::diagnostics::log(format_args!("capture_engine {}", self.diagnostic_status()));
        if let Ok(state) = self.inner.shared.state.try_lock() {
            crate::diagnostics::log(format_args!(
                "frame_snapshot observed={} cached_sequence={:?} age_ms={:?} interval_ms={:?}",
                self.inner.shared.observed_sequence.load(Ordering::Acquire),
                state.latest.as_ref().map(|frame| frame.sequence),
                state
                    .latest
                    .as_ref()
                    .map(|frame| frame.captured_at.elapsed().as_millis()),
                state
                    .last_frame_interval
                    .map(|interval| interval.as_millis())
            ));
        }
        let preflight = PreflightGuard::new(Arc::clone(&self.inner.shared));
        let latest_sequence = self.current_cached_sequence();
        // Bracket the requested frame with matching UIA observations. A UIA
        // read only after FrameArrived could describe a newer layout than the
        // pixels in that frame, even with identical window dimensions.
        let request_evidence = match authorization.with_current(|| Ok(())).and_then(|()| {
            let layout =
                crate::diagnostics::measure("uia_preflight", || self.confirm_request_layout())?;
            Ok(layout)
        }) {
            Ok(evidence) => evidence,
            Err(error) => {
                return CaptureOutcome {
                    result: Err(error),
                    replacement: None,
                    save_result: None,
                };
            }
        };
        let request_layout = request_evidence;
        let mut save_result = None;
        if let Some(sequence) = latest_sequence {
            match self.copy_current_validated(sequence, request_layout, |width, height, bytes| {
                save_result =
                    output_capture(authorization, width, height, bytes, save_to_screenshots)?;
                Ok(())
            }) {
                Ok(Some(mut receipt)) => {
                    receipt.latency = request_started.elapsed();
                    crate::diagnostics::log(format_args!("capture latest-frame fast path"));
                    return CaptureOutcome {
                        result: Ok(receipt),
                        replacement: None,
                        save_result,
                    };
                }
                Err(error) => {
                    return CaptureOutcome {
                        result: Err(error),
                        replacement: None,
                        save_result,
                    };
                }
                Ok(None) => {
                    crate::diagnostics::log(format_args!(
                        "capture_fallback reason=latest_frame_not_validated"
                    ));
                }
            }
        }
        let request = match self.arm_request(None) {
            Ok(request) => request,
            Err(error) => {
                return CaptureOutcome {
                    result: Err(error),
                    replacement: None,
                    save_result,
                };
            }
        };
        drop(preflight);
        let mut outcome = self.copy_with_recovery_armed(
            |width, height, bytes| {
                save_result =
                    output_capture(authorization, width, height, bytes, save_to_screenshots)?;
                Ok(())
            },
            |source| {
                Self::start_authorized_with(authorization, || {
                    Self::start_source_with_layout(
                        source,
                        request_layout,
                        self.inner.shared.layout.clone(),
                    )
                })
            },
            |cached| {
                authorization.with_current(|| Ok(()))?;
                require_frame_after_request(cached.rendered_at_100ns, request.requested_at_100ns)?;
                self.validate_cached_layout(cached, request_layout)
            },
            Some(request),
        );
        outcome.save_result = save_result;
        if let Ok(receipt) = outcome.result.as_mut() {
            // Include preflight and any recovery wait/startup, not just the
            // final successful attempt, in user-visible diagnostic latency.
            receipt.latency = request_started.elapsed();
        }
        outcome
    }

    // Snapshot only metadata. In particular, never hold the frame-state lock
    // across UIA preflight: FrameArrived must be able to invalidate this snapshot.
    fn current_cached_sequence(&self) -> Option<u64> {
        if self.inner.shared.source.is_local_monitor() {
            return None;
        }
        let state = self.inner.shared.state.lock().ok()?;
        let cached = state.latest.as_ref()?;
        (cached.sequence == self.inner.shared.observed_sequence.load(Ordering::Acquire))
            .then_some(cached.sequence)
    }

    fn copy_current_validated(
        &self,
        sequence: u64,
        current_layout: Option<RemoteLayout>,
        copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
    ) -> Result<Option<CaptureReceipt>> {
        self.copy_current_checked(
            sequence,
            current_layout,
            |layout| {
                let current = detect_remote_layout(
                    self.inner.shared.layout.as_ref(),
                    self.inner
                        .shared
                        .source
                        .remote_target_id()
                        .ok_or_else(|| anyhow!("共有対象がありません"))?,
                    layout.geometry.image_dimensions().0,
                    layout.geometry.image_dimensions().1,
                    Some(layout),
                    "output-fast",
                )?;
                require_matching_layout(Some(layout), current)
            },
            copy,
        )
    }

    fn copy_current_checked(
        &self,
        sequence: u64,
        current_layout: Option<RemoteLayout>,
        validate: impl FnOnce(RemoteLayout) -> Result<()>,
        mut copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
    ) -> Result<Option<CaptureReceipt>> {
        if self.inner.shared.source.is_local_monitor() || current_layout.is_none() {
            return Ok(None);
        }
        if self.inner.shared.stopped.load(Ordering::Acquire) {
            return Err(anyhow!("キャプチャは停止しています"));
        }
        if self
            .inner
            .control
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?
            .as_ref()
            .is_some_and(CaptureControl::is_finished)
        {
            crate::diagnostics::log(format_args!("fast_path_reject_reason=session_finished"));
            return Ok(None);
        }
        // UIA can block; perform it before taking the pixel-buffer lock, then
        // recheck the sequence, geometry and layout under that lock below.
        validate(current_layout.expect("remote layout checked"))?;
        let state = self
            .inner
            .shared
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;
        let Some(cached) = state.latest.as_ref() else {
            crate::diagnostics::log(format_args!("fast_path_reject_reason=cache_missing"));
            return Ok(None);
        };
        if cached.sequence != sequence
            || self.inner.shared.observed_sequence.load(Ordering::Acquire) != sequence
            || cached.remote_layout != current_layout
            || state.source_size != Some((cached.source_width, cached.source_height))
        {
            crate::diagnostics::log(format_args!(
                "fast_path_reject_reason=metadata_changed requested_sequence={sequence} cached_sequence={} observed_sequence={} layout_same={} size_same={}",
                cached.sequence,
                self.inner.shared.observed_sequence.load(Ordering::Acquire),
                cached.remote_layout == current_layout,
                state.source_size == Some((cached.source_width, cached.source_height))
            ));
            return Ok(None);
        }
        self.inner.shared.source.validate_remote_target()?;
        if self.inner.shared.stopped.load(Ordering::Acquire) {
            return Err(anyhow!("キャプチャは停止しています"));
        }
        // output_capture checks authorization at the clipboard write boundary.
        // No image allocation: retain the single cropped buffer under this lock.
        copy(cached.width, cached.height, &cached.bytes)?;
        Ok(Some(CaptureReceipt {
            screen_rect: cached.fallback_screen_rect,
            target_window_id: self.inner.shared.source.remote_target_id(),
            latency: Duration::ZERO,
            frame_age: cached.captured_at.elapsed(),
        }))
    }

    fn confirm_request_layout(&self) -> Result<Option<RemoteLayout>> {
        self.inner.shared.source.validate_remote_target()?;
        let Some(target_id) = self.inner.shared.source.remote_target_id() else {
            return Ok(None);
        };
        let (width, height, previous) = {
            let state = self
                .inner
                .shared
                .state
                .lock()
                .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;
            let (width, height) = state
                .source_dimensions()
                .ok_or_else(|| anyhow!("共有コンテンツを準備中です"))?;
            (width, height, state.confirmed_layout)
        };
        detect_remote_layout(
            self.inner.shared.layout.as_ref(),
            target_id,
            width,
            height,
            previous,
            "preflight",
        )
        .map(Some)
    }

    fn validate_cached_layout(
        &self,
        cached: &CachedFrame,
        request_layout: Option<RemoteLayout>,
    ) -> Result<()> {
        let Some(target_id) = self.inner.shared.source.remote_target_id() else {
            return Ok(());
        };
        let current = detect_remote_layout(
            self.inner.shared.layout.as_ref(),
            target_id,
            cached.source_width,
            cached.source_height,
            cached.remote_layout,
            "output",
        )?;
        require_matching_layout(request_layout, current)?;
        require_matching_layout(cached.remote_layout, current)
    }

    fn copy_with_recovery_armed(
        &self,
        mut copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
        restart: impl FnOnce(CaptureSource) -> Result<CaptureEngine>,
        mut validate: impl FnMut(&CachedFrame) -> Result<()>,
        request: Option<ArmedRequest>,
    ) -> CaptureOutcome {
        let first = self.copy_latest_armed(&mut copy, None, &mut validate, request);
        if first
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<FrameUnavailable>())
            .is_none()
        {
            return CaptureOutcome {
                result: first,
                replacement: None,
                save_result: None,
            };
        }

        let source = self.inner.shared.source.clone();
        if let Err(error) = source.validate_remote_target() {
            return CaptureOutcome {
                result: Err(error),
                replacement: None,
                save_result: None,
            };
        }
        // The old session cannot satisfy this request. Retire it before
        // starting its replacement, so backup UIA/crops do not compete with
        // the new session while the UI still retains the old engine handle.
        self.stop();
        let new_engine = match restart(source) {
            Ok(engine) => engine,
            Err(error) => {
                return CaptureOutcome {
                    result: Err(error),
                    replacement: None,
                    save_result: None,
                };
            }
        };
        let result = new_engine.copy_latest_armed(copy, Some(0), validate, None);
        CaptureOutcome {
            result,
            replacement: Some(new_engine),
            save_result: None,
        }
    }

    fn arm_request(&self, baseline_override: Option<u64>) -> Result<ArmedRequest> {
        let mut state = self
            .inner
            .shared
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;
        let baseline_sequence = baseline_override
            .unwrap_or_else(|| self.inner.shared.observed_sequence.load(Ordering::Acquire));
        let requested_at_100ns = performance_time_100ns()?;
        // Frame observation takes this same lock: no post-timestamp frame can
        // enter the baseline and be discarded as if it preceded the request.
        state.requested_after_sequence = baseline_sequence;
        self.inner
            .shared
            .capture_requested
            .store(true, Ordering::Release);
        Ok(ArmedRequest {
            baseline_sequence,
            requested_at_100ns,
        })
    }

    fn copy_latest_armed(
        &self,
        mut copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
        baseline_override: Option<u64>,
        mut validate: impl FnMut(&CachedFrame) -> Result<()>,
        request: Option<ArmedRequest>,
    ) -> Result<CaptureReceipt> {
        let started_at = Instant::now();
        let _request_guard = CaptureRequestGuard::new(Arc::clone(&self.inner.shared));
        if self.inner.shared.stopped.load(Ordering::Acquire) {
            return Err(anyhow!("キャプチャは停止しています"));
        }
        self.inner.shared.source.validate_remote_target()?;
        if self
            .inner
            .control
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?
            .as_ref()
            .is_some_and(CaptureControl::is_finished)
        {
            return Err(anyhow::Error::new(FrameUnavailable(
                "キャプチャセッションが終了しています",
            )));
        }
        let local_monitor = self.inner.shared.source.is_local_monitor();
        let request = match request {
            Some(request) => request,
            None => self.arm_request(baseline_override)?,
        };
        let state = self
            .inner
            .shared
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;
        let baseline_sequence = request.baseline_sequence;
        let had_frame = state.latest.is_some();
        let last_observed_at = state.last_observed_at;
        let last_frame_interval = state.last_frame_interval;
        drop(state);

        let timeout = if !had_frame {
            READY_TIMEOUT
        } else if local_monitor {
            LOCAL_FRESH_FRAME_WAIT
        } else {
            remote_frame_wait(last_observed_at, last_frame_interval, started_at)
        };
        let wait_started = Instant::now();
        let arrival_deadline = wait_started + timeout;
        let processing_deadline = wait_started + READY_TIMEOUT;
        let mut state = self
            .inner
            .shared
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;

        loop {
            if self.inner.shared.stopped.load(Ordering::Acquire) {
                return Err(anyhow!("キャプチャは停止しています"));
            }
            if state.last_error_sequence > baseline_sequence {
                return Err(anyhow!(
                    state
                        .last_error
                        .clone()
                        .unwrap_or_else(|| "共有画面の取得に失敗しました".to_string())
                ));
            }
            let has_fresh_frame = state
                .latest
                .as_ref()
                .is_some_and(|frame| frame.sequence > baseline_sequence);
            if has_fresh_frame {
                break;
            }

            let deadline = if self.inner.shared.observed_sequence.load(Ordering::Acquire)
                > baseline_sequence
            {
                processing_deadline
            } else {
                arrival_deadline
            };
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait_for = deadline.saturating_duration_since(now);
            let (next_state, _) = self
                .inner
                .shared
                .ready
                .wait_timeout(state, wait_for)
                .map_err(|_| anyhow!("キャプチャ状態の待機に失敗しました"))?;
            state = next_state;
        }

        let cached_sequence = state.latest.as_ref().map_or(0, |frame| frame.sequence);
        if local_monitor && cached_sequence <= baseline_sequence {
            return Err(anyhow::Error::new(FrameUnavailable(
                "自分の共有画面の新しいフレームを取得できませんでした",
            )));
        }
        if !local_monitor && cached_sequence <= baseline_sequence {
            return Err(anyhow::Error::new(FrameUnavailable(
                "新しい共有画面フレームをキャッシュできませんでした",
            )));
        }

        if let CaptureSource::LocalMonitor(target) = &self.inner.shared.source
            && !validate_local_monitor_target(target)?
        {
            return Err(anyhow!(
                "Teamsの共有対象が変わったため、誤撮影を防ぐために停止しました"
            ));
        }

        let cached = state.latest.as_ref().ok_or_else(|| {
            state.last_error.clone().map_or_else(
                || anyhow!("共有コンテンツを準備中です。少し待ってからもう一度撮影してください"),
                |message| anyhow!(message),
            )
        })?;
        // Dirty regions are rendering hints, not evidence that Teams kept the
        // sharing layout. Reconfirm UIA and the exact capture geometry for every
        // output. A static source recovers through a new WGC session, whose
        // initial frame is acquired after the request's UIA preflight.
        validate(cached)?;
        crate::diagnostics::log(format_args!(
            "capture validation complete elapsed_ms={}",
            started_at.elapsed().as_millis()
        ));
        if self.inner.shared.stopped.load(Ordering::Acquire) {
            return Err(anyhow!("キャプチャは停止しています"));
        }
        // The window can be minimized or closed while waiting for a new frame.
        // Validate at the write boundary instead of relying on meeting snapshots.
        self.inner.shared.source.validate_remote_target()?;
        copy(cached.width, cached.height, cached.bytes.as_slice())?;
        let content_rect = cached.content_rect;
        let source_width = cached.source_width;
        let source_height = cached.source_height;
        let fallback_screen_rect = cached.fallback_screen_rect;
        let frame_age = cached.captured_at.elapsed();
        drop(state);

        let screen_rect = self
            .inner
            .shared
            .source
            .remote_target_id()
            .and_then(|target_id| {
                current_screen_rect(target_id, content_rect, source_width, source_height)
            })
            .unwrap_or(fallback_screen_rect);

        Ok(CaptureReceipt {
            screen_rect,
            target_window_id: self.inner.shared.source.remote_target_id(),
            latency: started_at.elapsed(),
            frame_age,
        })
    }
}

impl GraphicsCaptureApiHandler for FrameHandler {
    type Flags = Arc<SharedState>;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            shared: ctx.flags,
            last_cache_update: None,
            last_detection: None,
            last_source_size: None,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        let now = Instant::now();
        let source_size = (frame.width(), frame.height());
        if self.shared.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let observed_sequence = self
            .shared
            .observe_frame(source_size, now)
            .map_err(|error| error.to_string())?;
        let requested = self.shared.capture_requested.load(Ordering::Acquire);
        if self.shared.backup_paused(requested) {
            return Ok(());
        }
        if let CaptureSource::LocalMonitor(target) = self.shared.source.clone() {
            return self.on_local_monitor_frame(frame, now, observed_sequence, requested, target);
        }
        let has_latest = self
            .shared
            .state
            .lock()
            .map(|state| state.latest.is_some())
            .unwrap_or(false);
        let size_changed = self.last_source_size != Some(source_size);
        if size_changed {
            self.last_source_size = Some(source_size);
        }
        let needs_cache = requested
            || size_changed
            || self
                .last_cache_update
                .is_none_or(|last| now.duration_since(last) >= BACKUP_CACHE_INTERVAL);
        let detection_due = requested
            || size_changed
            || has_latest
            || self
                .last_detection
                .is_none_or(|last| now.duration_since(last) >= DETECTION_RETRY_INTERVAL);
        if !needs_cache || !detection_due {
            return Ok(());
        }

        // Every CPU crop requires current authoritative UIA evidence. Absence
        // of dirty-region hints cannot prove that a side panel did not open.
        self.last_detection = Some(now);
        let result = self.detect_and_cache(frame, now, observed_sequence);

        match result {
            Ok(()) => {
                self.last_cache_update = Some(now);
            }
            Err(error) => {
                let message = error.to_string();
                if let Ok(mut state) = self.shared.state.lock() {
                    if state.latest.is_some() || state.last_error.as_ref() != Some(&message) {
                        crate::diagnostics::log(format_args!(
                            "remote frame unavailable: {message}"
                        ));
                    }
                    state.latest = None;
                    state.content_rect = None;
                    self.shared.has_cached_frame.store(false, Ordering::Release);
                    state.last_error = Some(message);
                    state.last_error_sequence = observed_sequence;
                }
                self.shared.ready.notify_all();
            }
        }

        Ok(())
    }

    fn on_closed(&mut self) -> std::result::Result<(), Self::Error> {
        if let Ok(mut state) = self.shared.state.lock() {
            state.latest = None;
            state.content_rect = None;
            state.last_error = Some("選択中のTeamsウィンドウが閉じられました".to_string());
            state.last_error_sequence = self
                .shared
                .observed_sequence
                .load(Ordering::Acquire)
                .wrapping_add(1);
            self.shared.has_cached_frame.store(false, Ordering::Release);
        }
        self.shared.ready.notify_all();
        Ok(())
    }
}

impl FrameHandler {
    fn on_local_monitor_frame(
        &mut self,
        frame: &mut Frame,
        captured_at: Instant,
        observed_sequence: u64,
        requested: bool,
        target: LocalMonitorCaptureTarget,
    ) -> std::result::Result<(), String> {
        let source_size = (frame.width(), frame.height());
        let has_latest = self
            .shared
            .state
            .lock()
            .map(|state| state.latest.is_some())
            .unwrap_or(false);
        let needs_cache = requested
            || !has_latest
            || self
                .last_cache_update
                .is_none_or(|last| captured_at.duration_since(last) >= BACKUP_CACHE_INTERVAL);
        if !needs_cache {
            return Ok(());
        }

        let result = (|| -> Result<()> {
            if source_size != (target.screen_rect.width, target.screen_rect.height) {
                return Err(anyhow!(
                    "共有モニターのサイズが変わったため再検出が必要です"
                ));
            }
            if !validate_local_monitor_target(&target)? {
                return Err(anyhow!("Teamsが共有しているモニターを確認できませんでした"));
            }

            self.cache_crop(
                frame,
                PixelRect::new(0, 0, source_size.0, source_size.1),
                captured_at,
                target.screen_rect,
                observed_sequence,
                None,
            )
        })();

        match result {
            Ok(()) => {
                self.last_source_size = Some(source_size);
                self.last_cache_update = Some(captured_at);
            }
            Err(error) => {
                if let Ok(mut state) = self.shared.state.lock() {
                    if state.latest.is_some()
                        || state.last_error.as_deref() != Some(error.to_string().as_str())
                    {
                        crate::diagnostics::log(format_args!(
                            "local_frame_unavailable error={error}"
                        ));
                    }
                    state.latest = None;
                    state.content_rect = None;
                    state.last_error = Some(error.to_string());
                    state.last_error_sequence = observed_sequence;
                    self.shared.has_cached_frame.store(false, Ordering::Release);
                }
                self.shared.ready.notify_all();
            }
        }
        Ok(())
    }

    fn detect_and_cache(
        &mut self,
        frame: &mut Frame,
        captured_at: Instant,
        observed_sequence: u64,
    ) -> Result<()> {
        let source_width = frame.width();
        let source_height = frame.height();
        let target_id = self
            .shared
            .source
            .remote_target_id()
            .ok_or_else(|| anyhow!("Teams会議ウィンドウの対象がありません"))?;
        let previous = self
            .shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.confirmed_layout);
        let layout = detect_remote_layout(
            self.shared.layout.as_ref(),
            target_id,
            source_width,
            source_height,
            previous,
            "frame",
        )?;
        let screen_rect = layout
            .geometry
            .map_pixel_rect_to_screen(layout.content_rect)
            .ok_or_else(|| anyhow!("共有コンテンツの画面座標を計算できませんでした"))?;
        self.cache_crop(
            frame,
            layout.content_rect,
            captured_at,
            screen_rect,
            observed_sequence,
            Some(layout),
        )
    }

    fn cache_crop(
        &mut self,
        frame: &mut Frame,
        content_rect: PixelRect,
        captured_at: Instant,
        fallback_screen_rect: ScreenRect,
        sequence: u64,
        remote_layout: Option<RemoteLayout>,
    ) -> Result<()> {
        let crop_started = Instant::now();
        let source_width = frame.width();
        let source_height = frame.height();
        let rendered_at_100ns = frame
            .timestamp()
            .context("フレームの描画時刻を取得できませんでした")?
            .Duration;
        if content_rect.x.saturating_add(content_rect.width) > source_width
            || content_rect.y.saturating_add(content_rect.height) > source_height
        {
            return Err(anyhow!("Teamsのレイアウト変更を検出しました"));
        }

        let mut buffer = frame
            .buffer_crop(
                content_rect.x,
                content_rect.y,
                content_rect.x + content_rect.width,
                content_rect.y + content_rect.height,
            )
            .context("最新の共有画面フレームを取得できませんでした")?;
        let row_pitch = buffer.row_pitch() as usize;
        let row_bytes = content_rect.width as usize * 4;
        let expected_len = row_bytes * content_rect.height as usize;
        let raw = buffer.as_raw_buffer();
        let required_raw_len = row_pitch
            .saturating_mul(content_rect.height.saturating_sub(1) as usize)
            .saturating_add(row_bytes);
        if raw.len() < required_raw_len {
            return Err(anyhow!("共有画面フレームのバッファサイズが不正です"));
        }

        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を更新できませんでした"))?;
        let mut bytes = state
            .latest
            .take()
            .map(|frame| frame.bytes)
            .unwrap_or_default();
        if bytes.capacity() > expected_len.saturating_mul(2) {
            bytes = Vec::with_capacity(expected_len);
        }
        bytes.resize(expected_len, 0);
        if row_pitch == row_bytes {
            bytes.copy_from_slice(&raw[..expected_len]);
        } else {
            for row in 0..content_rect.height as usize {
                let source_start = row * row_pitch;
                let target_start = row * row_bytes;
                bytes[target_start..target_start + row_bytes]
                    .copy_from_slice(&raw[source_start..source_start + row_bytes]);
            }
        }

        state.latest = Some(CachedFrame {
            width: content_rect.width,
            height: content_rect.height,
            bytes,
            captured_at,
            content_rect,
            fallback_screen_rect,
            source_width,
            source_height,
            sequence,
            remote_layout,
            rendered_at_100ns,
        });
        state.content_rect = Some(content_rect);
        state.confirmed_layout = remote_layout;
        state.last_error = None;
        state.last_error_sequence = 0;
        // A crop already in flight when the user clicked cannot consume the
        // new request. Keep refreshing until a subsequent frame is cached.
        if sequence > state.requested_after_sequence {
            self.shared
                .capture_requested
                .store(false, Ordering::Release);
        }
        self.shared.has_cached_frame.store(true, Ordering::Release);
        drop(state);
        self.shared.ready.notify_all();
        crate::diagnostics::log(format_args!(
            "frame_cached sequence={sequence} source={}x{} crop={content_rect:?} gpu_cpu_us={} frame_age_ms={}",
            source_width,
            source_height,
            crop_started.elapsed().as_micros(),
            captured_at.elapsed().as_millis()
        ));
        Ok(())
    }
}

fn remote_frame_wait(
    last_observed_at: Option<Instant>,
    last_frame_interval: Option<Duration>,
    now: Instant,
) -> Duration {
    // A low-rate source may have delivered a frame during UIA preflight,
    // making its last-arrival age small even though the next update is far
    // away. Avoid spending the entire wait budget before the same recovery.
    // Cadence only chooses when to restart; it never authorizes cached pixels.
    if let (Some(last), Some(interval)) = (last_observed_at, last_frame_interval)
        && interval.saturating_sub(now.saturating_duration_since(last)) > FRESH_FRAME_WAIT
    {
        return Duration::ZERO;
    }
    // A quiescent WGC source may not emit until its pixels change. If it has
    // already been silent for the entire frame budget, recover immediately
    // instead of paying that budget again on every click. This only changes
    // when recovery starts; output still requires new, timestamp-checked pixels.
    if last_observed_at.is_some_and(|last| now.saturating_duration_since(last) >= FRESH_FRAME_WAIT)
    {
        Duration::ZERO
    } else {
        FRESH_FRAME_WAIT
    }
}

// WGC SystemRelativeTime is QPC time expressed in 100 ns units. Comparing
// compositor timestamps also rejects old frames delivered after the request.
fn performance_time_100ns() -> Result<i64> {
    let mut counter = 0;
    let mut frequency = 0;
    unsafe {
        QueryPerformanceCounter(&mut counter)?;
        QueryPerformanceFrequency(&mut frequency)?;
    }
    if counter < 0 || frequency <= 0 {
        return Err(anyhow!("キャプチャの時刻を確認できませんでした"));
    }
    i64::try_from(i128::from(counter) * 10_000_000 / i128::from(frequency))
        .context("キャプチャの時刻を変換できませんでした")
}

fn require_frame_after_request(rendered_at_100ns: i64, requested_at_100ns: i64) -> Result<()> {
    if rendered_at_100ns <= requested_at_100ns {
        return Err(anyhow::Error::new(FrameUnavailable(
            "UIA確認後に描画されたフレームが必要です",
        )));
    }
    Ok(())
}

fn detect_remote_layout(
    resolver: Option<&LayoutResolver>,
    _target_id: u32,
    source_width: u32,
    source_height: u32,
    previous: Option<RemoteLayout>,
    phase: &str,
) -> Result<RemoteLayout> {
    let started = Instant::now();
    let current = crate::diagnostics::measure(phase, || {
        resolver
            .ok_or_else(|| anyhow!("共有範囲の監視がありません"))?
            .verify(source_width, source_height, phase)
    })?;
    crate::diagnostics::log(format_args!(
        "uia layout phase={phase} elapsed_ms={} reused={}",
        started.elapsed().as_millis(),
        previous == Some(current)
    ));
    Ok(current)
}

fn require_matching_layout(cached: Option<RemoteLayout>, current: RemoteLayout) -> Result<()> {
    if cached != Some(current) {
        return Err(anyhow!(
            "Teamsの共有範囲または配置が変わったため、もう一度撮影してください"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::uia::WindowGeometry;
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_MINIMIZE,
            WS_OVERLAPPEDWINDOW,
        },
        core::w,
    };

    impl CaptureEngine {
        fn copy_latest_since_checked(
            &self,
            copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
            baseline: Option<u64>,
            validate: impl FnMut(&CachedFrame) -> Result<()>,
        ) -> Result<CaptureReceipt> {
            self.copy_latest_armed(copy, baseline, validate, None)
        }

        fn copy_with_recovery_checked(
            &self,
            copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
            restart: impl FnOnce(CaptureSource) -> Result<CaptureEngine>,
            validate: impl FnMut(&CachedFrame) -> Result<()>,
        ) -> CaptureOutcome {
            self.copy_with_recovery_armed(copy, restart, validate, None)
        }

        fn copy_latest_with(
            &self,
            copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
        ) -> Result<CaptureReceipt> {
            self.copy_latest_since_checked(copy, None, |_| Ok(()))
        }

        fn copy_latest_since(
            &self,
            copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
            baseline: Option<u64>,
        ) -> Result<CaptureReceipt> {
            self.copy_latest_since_checked(copy, baseline, |_| Ok(()))
        }

        fn copy_with_recovery(
            &self,
            copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
            restart: impl FnOnce(CaptureSource) -> Result<CaptureEngine>,
        ) -> CaptureOutcome {
            self.copy_with_recovery_checked(copy, restart, |_| Ok(()))
        }
    }

    struct TestWindow(HWND);

    impl TestWindow {
        fn new(minimized: bool) -> Self {
            let style = if minimized {
                WS_OVERLAPPEDWINDOW | WS_MINIMIZE
            } else {
                WS_OVERLAPPEDWINDOW
            };
            // Keep test windows hidden and non-activating.
            Self(
                unsafe {
                    CreateWindowExW(
                        WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                        w!("STATIC"),
                        w!("Snapbar capture regression"),
                        style,
                        0,
                        0,
                        100,
                        100,
                        None,
                        None,
                        None,
                        None,
                    )
                }
                .expect("create hidden test window"),
            )
        }

        fn target_id(&self) -> u32 {
            self.0.0 as usize as u32
        }
    }

    impl Drop for TestWindow {
        fn drop(&mut self) {
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    fn cached_remote_engine(target_id: u32) -> CaptureEngine {
        let rect = PixelRect::new(0, 0, 1, 1);
        CaptureEngine {
            inner: Arc::new(EngineInner {
                shared: Arc::new(SharedState {
                    source: CaptureSource::RemoteTeamsWindow(target_id),
                    layout: None,
                    stopped: AtomicBool::new(false),
                    preflight_in_progress: AtomicBool::new(false),
                    capture_requested: AtomicBool::new(false),
                    observed_sequence: std::sync::atomic::AtomicU64::new(1),
                    has_cached_frame: AtomicBool::new(true),
                    state: Mutex::new(RuntimeState {
                        latest: Some(CachedFrame {
                            width: 1,
                            height: 1,
                            bytes: vec![10, 20, 30, 255],
                            captured_at: Instant::now() - Duration::from_secs(60),
                            content_rect: rect,
                            fallback_screen_rect: ScreenRect {
                                x: 0,
                                y: 0,
                                width: 1,
                                height: 1,
                            },
                            source_width: 1,
                            source_height: 1,
                            sequence: 1,
                            remote_layout: None,
                            rendered_at_100ns: 0,
                        }),
                        content_rect: Some(rect),
                        source_size: Some((1, 1)),
                        confirmed_layout: None,
                        last_observed_at: None,
                        last_frame_interval: None,
                        last_error: None,
                        last_error_sequence: 0,
                        requested_after_sequence: 0,
                    }),
                    ready: Condvar::new(),
                }),
                control: Mutex::new(None),
            }),
        }
    }

    #[test]
    fn latest_frame_fast_path_rechecks_observations_layout_and_stop() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let layout = RemoteLayout {
            revision: 0,
            geometry: WindowGeometry::from_screen_rect(
                ScreenRect {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
                1,
                1,
            ),
            content_rect: PixelRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
        };
        engine
            .inner
            .shared
            .state
            .lock()
            .unwrap()
            .latest
            .as_mut()
            .unwrap()
            .remote_layout = Some(layout);
        let sequence = engine.current_cached_sequence().unwrap();
        let mut copies = 0;
        assert!(
            engine
                .copy_current_checked(
                    sequence,
                    Some(layout),
                    |_| Err(anyhow!("output evidence invalidated")),
                    |_, _, _| panic!("invalid evidence must not write clipboard or PNG")
                )
                .is_err()
        );
        // A static frame can be old in wall-clock time while still being the
        // latest observed frame. It must not require a session restart.
        assert!(
            engine
                .copy_current_checked(
                    sequence,
                    Some(layout),
                    |_| Ok(()),
                    |_, _, bytes| {
                        assert_eq!(bytes, &[10, 20, 30, 255]);
                        copies += 1;
                        Ok(())
                    }
                )
                .unwrap()
                .is_some()
        );
        let changed = RemoteLayout {
            content_rect: PixelRect {
                x: 1,
                ..layout.content_rect
            },
            ..layout
        };
        assert!(
            engine
                .copy_current_checked(
                    sequence,
                    Some(changed),
                    |_| Ok(()),
                    |_, _, _| panic!("changed crop")
                )
                .unwrap()
                .is_none()
        );
        // This can run while UIA validates because the metadata snapshot holds
        // no mutex guard. An un-cached newer arrival invalidates the fast path.
        assert!(
            engine
                .copy_current_checked(
                    sequence,
                    Some(layout),
                    |_| {
                        // The live UIA check must run without the pixel mutex held.
                        assert!(engine.inner.shared.state.try_lock().is_ok());
                        engine
                            .inner
                            .shared
                            .observe_frame((1, 1), Instant::now())
                            .unwrap();
                        Ok(())
                    },
                    |_, _, _| panic!("frame changed during output validation")
                )
                .unwrap()
                .is_none()
        );
        assert!(engine.current_cached_sequence().is_none());
        assert!(
            engine
                .copy_current_checked(
                    sequence,
                    Some(layout),
                    |_| Ok(()),
                    |_, _, _| panic!("stale frame")
                )
                .unwrap()
                .is_none()
        );
        engine.stop();
        assert!(
            engine
                .copy_current_checked(
                    sequence,
                    Some(layout),
                    |_| Ok(()),
                    |_, _, _| panic!("stopped")
                )
                .is_err()
        );
        assert_eq!(copies, 1);
    }

    #[test]
    fn revocation_during_startup_does_not_block_and_stops_result() {
        use std::sync::mpsc;
        let authorization = CaptureAuthorization::new();
        let worker_authorization = authorization.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let retained = cached_remote_engine(0);
        let worker_engine = retained.clone();
        let worker = std::thread::spawn(move || {
            CaptureEngine::start_authorized_with(&worker_authorization, || {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(worker_engine)
            })
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        authorization.invalidate();
        release_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_err());
        assert!(retained.is_finished());
        assert!(!retained.is_ready());
    }

    #[test]
    fn stopped_engine_does_not_report_ready_or_copy_retained_frame() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        assert!(engine.is_ready());
        engine.stop();
        assert!(!engine.is_ready());
        assert!(engine.is_finished());
        let result = engine.copy_latest_since(
            |_, _, _| panic!("stopped capture must not publish"),
            Some(0),
        );
        assert!(result.unwrap_err().to_string().contains("停止"));
    }

    #[test]
    fn minimized_remote_capture_preserves_clipboard() {
        let window = TestWindow::new(true);
        assert!(unsafe { IsIconic(window.0).as_bool() });
        let engine = cached_remote_engine(window.target_id());
        assert!(engine.is_ready());
        let mut clipboard = vec![99];

        let error = engine
            .copy_latest_with(|_, _, bytes| {
                clipboard = bytes.to_vec();
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("最小化"));
        assert_eq!(clipboard, [99]);
        assert!(engine.is_ready());
        assert!(
            !engine
                .inner
                .shared
                .capture_requested
                .load(Ordering::Acquire)
        );
    }

    #[test]
    fn minimized_remote_recovery_does_not_retry_or_replace() {
        let window = TestWindow::new(true);
        let engine = cached_remote_engine(window.target_id());
        let outcome = engine.copy_with_recovery(
            |_, _, _| panic!("minimized capture must not write to clipboard"),
            |_| panic!("minimized capture must not restart"),
        );

        assert!(outcome.result.unwrap_err().to_string().contains("最小化"));
        assert!(outcome.replacement.is_none());
    }

    #[test]
    fn remote_capture_does_not_require_foreground_window() {
        let window = TestWindow::new(false);
        assert!(!unsafe { IsIconic(window.0).as_bool() });
        let engine = cached_remote_engine(window.target_id());
        engine
            .inner
            .shared
            .observed_sequence
            .store(0, Ordering::Release);
        let mut clipboard = vec![99];
        engine
            .copy_latest_with(|width, height, bytes| {
                assert_eq!((width, height), (1, 1));
                clipboard = bytes.to_vec();
                Ok(())
            })
            .unwrap();
        assert_eq!(clipboard, [10, 20, 30, 255]);
    }

    #[test]
    fn missing_remote_window_does_not_copy_cached_frame() {
        let engine = cached_remote_engine(0);
        let error = engine
            .copy_latest_with(|_, _, _| panic!("missing window must not write to clipboard"))
            .unwrap_err();
        assert!(error.to_string().contains("見つかりません"));
    }

    #[test]
    fn shrinking_share_at_unchanged_frame_size_preserves_clipboard() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let geometry = WindowGeometry::from_screen_rect(
            ScreenRect {
                x: 0,
                y: 0,
                width: 1600,
                height: 900,
            },
            1600,
            900,
        );
        let previous = RemoteLayout {
            revision: 0,
            geometry,
            content_rect: PixelRect::new(100, 100, 1400, 750),
        };
        let current = RemoteLayout {
            revision: 0,
            geometry,
            content_rect: PixelRect::new(100, 100, 1100, 750),
        };
        engine
            .inner
            .shared
            .state
            .lock()
            .unwrap()
            .latest
            .as_mut()
            .unwrap()
            .remote_layout = Some(previous);
        let mut clipboard = vec![99];
        let result = engine.copy_latest_since_checked(
            |_, _, bytes| {
                clipboard = bytes.to_vec();
                Ok(())
            },
            Some(0),
            |cached| require_matching_layout(cached.remote_layout, current),
        );
        assert!(result.unwrap_err().to_string().contains("共有範囲"));
        assert_eq!(clipboard, [99]);
    }

    #[test]
    fn unavailable_uia_never_falls_back_to_a_confirmed_crop() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        engine
            .inner
            .shared
            .observed_sequence
            .store(0, Ordering::Release);
        let outcome = engine.copy_with_recovery_checked(
            |_, _, _| panic!("unconfirmed layout must not reach output"),
            |_| panic!("UIA failure must not restart and reuse the crop"),
            |_| Err(anyhow!("UIA unavailable")),
        );
        assert_eq!(outcome.result.unwrap_err().to_string(), "UIA unavailable");
        assert!(outcome.replacement.is_none());
    }

    #[test]
    fn same_crop_with_changed_window_geometry_is_rejected() {
        let rect = ScreenRect {
            x: 0,
            y: 0,
            width: 1600,
            height: 900,
        };
        let previous = RemoteLayout {
            revision: 0,
            geometry: WindowGeometry::from_screen_rect(rect, 1600, 900),
            content_rect: PixelRect::new(100, 100, 1400, 750),
        };
        for rect in [
            ScreenRect { x: 20, ..rect },
            ScreenRect {
                width: 1800,
                ..rect
            },
        ] {
            let current = RemoteLayout {
                geometry: WindowGeometry::from_screen_rect(rect, 1600, 900),
                ..previous
            };
            assert!(require_matching_layout(Some(previous), current).is_err());
        }
        assert!(require_matching_layout(Some(previous), previous).is_ok());
        assert!(require_matching_layout(None, previous).is_err());
    }

    #[test]
    fn share_ending_while_waiting_for_a_frame_prevents_output() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let authorization = CaptureAuthorization::default();
        let worker_auth = authorization.clone();
        let shared = Arc::clone(&engine.inner.shared);
        let producer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !shared.capture_requested.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            // Simulate the UI receiving sharing-end while this worker retains
            // the old engine, then an already queued WGC frame arriving.
            worker_auth.invalidate();
            let mut state = shared.state.lock().unwrap();
            state.latest.as_mut().unwrap().sequence = 2;
            shared.observed_sequence.store(2, Ordering::Release);
            drop(state);
            shared.ready.notify_all();
        });
        let mut output_calls = 0;
        let result = engine.copy_latest_since_checked(
            |_, _, _| {
                authorization.with_current(|| {
                    output_calls += 1;
                    Ok(())
                })
            },
            None,
            |_| Ok(()),
        );
        producer.join().unwrap();
        assert!(result.is_err());
        assert_eq!(output_calls, 0);
    }

    #[test]
    fn late_delivery_of_preflight_pixels_never_reaches_output() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let result = engine.copy_latest_since_checked(
            |_, _, _| panic!("a later sequence does not prove when the pixels were drawn"),
            Some(0),
            |cached| require_frame_after_request(cached.rendered_at_100ns, 10),
        );
        assert!(
            result
                .unwrap_err()
                .downcast_ref::<FrameUnavailable>()
                .is_some()
        );
        assert!(require_frame_after_request(10, 10).is_err());
        assert!(require_frame_after_request(11, 10).is_ok());
    }

    #[test]
    fn remote_static_cache_requires_a_frame_after_the_request() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let error = engine
            .copy_latest_with(|_, _, _| panic!("pre-request pixels must not be copied"))
            .unwrap_err();
        assert!(error.downcast_ref::<FrameUnavailable>().is_some());
    }

    #[test]
    fn quiescent_source_skips_wait_without_authorizing_old_pixels() {
        let now = Instant::now();
        assert_eq!(remote_frame_wait(None, None, now), FRESH_FRAME_WAIT);
        assert_eq!(remote_frame_wait(Some(now), None, now), FRESH_FRAME_WAIT);
        assert_eq!(
            remote_frame_wait(Some(now - FRESH_FRAME_WAIT), None, now),
            Duration::ZERO
        );
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        engine.inner.shared.state.lock().unwrap().last_observed_at =
            Some(now - Duration::from_secs(1));
        let outcome = engine.copy_with_recovery(
            |_, _, _| panic!("quiescence does not authorize old pixels"),
            |_| Err(anyhow!("fresh session unavailable")),
        );
        assert_eq!(
            outcome.result.unwrap_err().to_string(),
            "fresh session unavailable"
        );
        assert!(outcome.replacement.is_none());
    }

    #[test]
    fn low_rate_frame_during_preflight_skips_wait_but_still_requires_new_pixels() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let now = Instant::now();
        engine
            .inner
            .shared
            .observe_frame((1, 1), now - Duration::from_secs(1))
            .unwrap();
        engine.inner.shared.observe_frame((1, 1), now).unwrap();
        let interval = engine
            .inner
            .shared
            .state
            .lock()
            .unwrap()
            .last_frame_interval;
        assert_eq!(remote_frame_wait(Some(now), interval, now), Duration::ZERO);
        assert_eq!(
            remote_frame_wait(Some(now), Some(Duration::from_millis(16)), now),
            FRESH_FRAME_WAIT
        );
        let outcome = engine.copy_with_recovery(
            |_, _, _| panic!("cadence must not authorize old pixels"),
            |_| Err(anyhow!("fresh session unavailable")),
        );
        assert_eq!(
            outcome.result.unwrap_err().to_string(),
            "fresh session unavailable"
        );
    }

    #[test]
    fn preflight_pauses_backup_without_consuming_the_capture_request() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let shared = &engine.inner.shared;
        {
            let _preflight = PreflightGuard::new(Arc::clone(shared));
            assert!(shared.backup_paused(false));
            assert!(!shared.capture_requested.load(Ordering::Acquire));
            shared.observe_frame((20, 30), Instant::now()).unwrap();
            assert_eq!(shared.state.lock().unwrap().source_size, Some((20, 30)));
            assert!(!shared.backup_paused(true));
        }
        assert!(!shared.backup_paused(false));
        assert!(!shared.capture_requested.load(Ordering::Acquire));
    }

    #[test]
    fn frame_between_arming_and_waiting_is_not_swallowed_by_a_new_baseline() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let request = engine.arm_request(None).unwrap();
        let sequence = engine
            .inner
            .shared
            .observe_frame((1, 1), Instant::now())
            .unwrap();
        assert!(sequence > request.baseline_sequence);
        {
            let mut state = engine.inner.shared.state.lock().unwrap();
            assert_eq!(state.requested_after_sequence, request.baseline_sequence);
            let frame = state.latest.as_mut().unwrap();
            frame.sequence = sequence;
            frame.rendered_at_100ns = request.requested_at_100ns + 1;
            frame.bytes[0] = 77;
        }
        let mut copied = 0;
        engine
            .copy_latest_armed(
                |_, _, bytes| {
                    copied = bytes[0];
                    Ok(())
                },
                None,
                |frame| {
                    require_frame_after_request(frame.rendered_at_100ns, request.requested_at_100ns)
                },
                Some(request),
            )
            .unwrap();
        assert_eq!(copied, 77);
        assert!(
            !engine
                .inner
                .shared
                .capture_requested
                .load(Ordering::Acquire)
        );
    }

    #[test]
    fn remote_copy_waits_for_each_80ms_frame_at_4hz() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let shared = Arc::clone(&engine.inner.shared);
        let producer = std::thread::spawn(move || {
            for value in 1_u8..=8 {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !shared.capture_requested.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline, "capture request never arrived");
                    std::thread::sleep(Duration::from_millis(1));
                }
                std::thread::sleep(Duration::from_millis(80));
                let sequence = shared
                    .observed_sequence
                    .fetch_add(1, Ordering::AcqRel)
                    .wrapping_add(1);
                let mut state = shared.state.lock().unwrap();
                let latest = state.latest.as_mut().unwrap();
                latest.bytes[0] = value;
                latest.sequence = sequence;
                latest.captured_at = Instant::now();
                shared.capture_requested.store(false, Ordering::Release);
                drop(state);
                shared.ready.notify_all();
            }
        });

        let started = Instant::now();
        for value in 1_u8..=8 {
            let next_capture = started + Duration::from_millis(u64::from(value - 1) * 250);
            std::thread::sleep(next_capture.saturating_duration_since(Instant::now()));
            let mut copied = 0;
            engine
                .copy_latest_with(|_, _, bytes| {
                    copied = bytes[0];
                    Ok(())
                })
                .unwrap();
            assert_eq!(copied, value);
        }
        producer.join().unwrap();
    }

    #[test]
    fn remote_copy_rejects_stale_cache_after_an_observed_frame() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        engine
            .inner
            .shared
            .observed_sequence
            .store(1, Ordering::Release);
        let shared = Arc::clone(&engine.inner.shared);
        let producer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !shared.capture_requested.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "capture request never arrived");
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(80));
            shared.observed_sequence.store(2, Ordering::Release);
            shared.ready.notify_all();
        });

        let mut copied = false;
        let result = engine.copy_latest_with(|_, _, _| {
            copied = true;
            Ok(())
        });
        producer.join().unwrap();
        assert!(result.is_err());
        assert!(!copied);
        assert!(
            !engine
                .inner
                .shared
                .capture_requested
                .load(Ordering::Acquire)
        );
    }

    #[test]
    fn frame_unavailable_restarts_once_and_returns_replacement() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        engine
            .inner
            .shared
            .observed_sequence
            .store(2, Ordering::Release);
        let replacement = cached_remote_engine(window.target_id());
        replacement
            .inner
            .shared
            .state
            .lock()
            .unwrap()
            .latest
            .as_mut()
            .unwrap()
            .bytes[0] = 77;

        let mut copied = 0;
        let outcome = engine.copy_with_recovery(
            |_, _, bytes| {
                copied = bytes[0];
                Ok(())
            },
            |_| {
                assert!(engine.is_finished());
                assert!(!engine.is_ready());
                Ok(replacement)
            },
        );

        assert!(outcome.result.is_ok());
        assert_eq!(copied, 77);
        assert!(outcome.replacement.is_some());
    }

    #[test]
    fn clipboard_error_does_not_restart_capture() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        engine
            .inner
            .shared
            .observed_sequence
            .store(0, Ordering::Release);
        let mut restarts = 0;
        let outcome = engine.copy_with_recovery(
            |_, _, _| Err(anyhow!("clipboard failed")),
            |_| {
                restarts += 1;
                Ok(cached_remote_engine(window.target_id()))
            },
        );

        assert!(outcome.result.is_err());
        assert_eq!(restarts, 0);
        assert!(outcome.replacement.is_none());
    }

    #[test]
    fn restart_failure_is_returned_without_a_replacement() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        engine
            .inner
            .shared
            .observed_sequence
            .store(2, Ordering::Release);
        let outcome =
            engine.copy_with_recovery(|_, _, _| Ok(()), |_| Err(anyhow!("restart failed")));

        assert_eq!(outcome.result.unwrap_err().to_string(), "restart failed");
        assert!(outcome.replacement.is_none());
        assert!(engine.is_finished());
    }

    #[test]
    fn failed_retry_returns_the_replacement_engine() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        engine
            .inner
            .shared
            .observed_sequence
            .store(2, Ordering::Release);
        let replacement = cached_remote_engine(window.target_id());
        replacement
            .inner
            .shared
            .observed_sequence
            .store(2, Ordering::Release);
        replacement
            .inner
            .shared
            .state
            .lock()
            .unwrap()
            .latest
            .as_mut()
            .unwrap()
            .sequence = 0;
        let outcome = engine.copy_with_recovery(|_, _, _| Ok(()), |_| Ok(replacement));

        assert!(outcome.result.is_err());
        assert!(outcome.replacement.is_some());
    }

    #[test]
    fn restarted_session_accepts_its_initial_cached_frame() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let started = Instant::now();
        let result = engine.copy_latest_since(|_, _, _| Ok(()), Some(0));

        assert!(result.is_ok());
        assert!(started.elapsed() < FRESH_FRAME_WAIT);
    }

    #[test]
    fn local_copy_requires_a_new_cache_even_if_another_frame_arrived() {
        let mut engine = cached_remote_engine(0);
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
        let shared = Arc::clone(&engine.inner.shared);
        let producer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !shared.capture_requested.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "capture request never arrived");
                std::thread::sleep(Duration::from_millis(1));
            }
            shared.observed_sequence.store(2, Ordering::Release);
            shared.ready.notify_all();
        });
        let result =
            engine.copy_latest_with(|_, _, _| panic!("stale local frame must not be copied"));
        producer.join().unwrap();
        assert!(result.unwrap_err().to_string().contains("新しいフレーム"));
        assert!(
            !engine
                .inner
                .shared
                .capture_requested
                .load(Ordering::Acquire)
        );
    }
    #[test]
    fn arrived_frame_finishes_uia_after_the_arrival_timeout_without_restart() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let shared = Arc::clone(&engine.inner.shared);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !shared.capture_requested.load(Ordering::Acquire) {
                    assert!(
                        Instant::now() < deadline,
                        "capture request was not published"
                    );
                    std::thread::yield_now();
                }
                shared.observed_sequence.store(2, Ordering::Release);
                // Exceed the 200 ms arrival budget while processing a real arrival.
                std::thread::sleep(Duration::from_millis(300));
                let mut state = shared.state.lock().unwrap();
                let cached = state.latest.as_mut().unwrap();
                cached.sequence = 2;
                cached.bytes = vec![40, 50, 60, 255];
                drop(state);
                shared.ready.notify_all();
            });
            let mut copied = Vec::new();
            let outcome = engine.copy_with_recovery(
                |_, _, bytes| {
                    copied = bytes.to_vec();
                    Ok(())
                },
                |_| panic!("a healthy in-flight crop must not restart WGC"),
            );
            outcome.result.unwrap();
            assert!(outcome.replacement.is_none());
            assert_eq!(copied, [40, 50, 60, 255]);
        });
    }
    #[test]
    fn fresh_uia_failure_is_reported_without_a_timeout_or_session_restart() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let shared = Arc::clone(&engine.inner.shared);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !shared.capture_requested.load(Ordering::Acquire) {
                    assert!(
                        Instant::now() < deadline,
                        "capture request was not published"
                    );
                    std::thread::yield_now();
                }
                shared.observed_sequence.store(2, Ordering::Release);
                let mut state = shared.state.lock().unwrap();
                state.latest = None;
                state.last_error = Some("test: authoritative UIA unavailable".to_string());
                state.last_error_sequence = 2;
                drop(state);
                shared.ready.notify_all();
            });
            let outcome = engine.copy_with_recovery(
                |_, _, _| panic!("unconfirmed pixels must not be copied"),
                |_| panic!("a UIA failure is not a dead WGC session"),
            );
            assert_eq!(
                outcome.result.unwrap_err().to_string(),
                "test: authoritative UIA unavailable"
            );
            assert!(outcome.replacement.is_none());
        });
    }
    #[test]
    fn lost_crop_keeps_only_source_dimensions_for_a_new_preflight() {
        let state = RuntimeState {
            source_size: Some((1920, 1080)),
            last_error: Some("transient UIA failure".to_string()),
            ..RuntimeState::default()
        };
        assert_eq!(state.source_dimensions(), Some((1920, 1080)));
        assert!(state.latest.is_none());
        assert!(state.content_rect.is_none());
        assert_eq!(RuntimeState::default().source_dimensions(), None);
    }
    #[test]
    fn explicit_retry_can_recover_an_empty_pixel_cache() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        {
            let mut state = engine.inner.shared.state.lock().unwrap();
            state.source_size = Some((1, 1));
            state.latest = None;
            state.last_error = Some("previous UIA failure".to_string());
            state.last_error_sequence = 1;
        }
        engine
            .inner
            .shared
            .has_cached_frame
            .store(false, Ordering::Release);
        let mut copied = false;
        let outcome = engine.copy_with_recovery(
            |_, _, _| {
                copied = true;
                Ok(())
            },
            |_| Ok(cached_remote_engine(window.target_id())),
        );
        outcome.result.unwrap();
        assert!(copied);
        assert!(outcome.replacement.is_some());
    }
    #[test]
    fn in_flight_crop_has_a_fixed_total_deadline() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let shared = Arc::clone(&engine.inner.shared);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !shared.capture_requested.load(Ordering::Acquire) {
                    assert!(
                        Instant::now() < deadline,
                        "capture request was not published"
                    );
                    std::thread::yield_now();
                }
                shared.observed_sequence.store(2, Ordering::Release);
            });
            let started = Instant::now();
            let error = engine
                .copy_latest_with(|_, _, _| panic!("no fresh crop"))
                .unwrap_err();
            assert!(error.downcast_ref::<FrameUnavailable>().is_some());
            assert!(started.elapsed() >= READY_TIMEOUT);
            assert!(
                !engine
                    .inner
                    .shared
                    .capture_requested
                    .load(Ordering::Acquire)
            );
        });
    }
    #[test]
    fn empty_cache_waits_past_an_old_in_flight_crop_for_the_requested_frame() {
        let window = TestWindow::new(false);
        let engine = cached_remote_engine(window.target_id());
        let old = engine
            .inner
            .shared
            .state
            .lock()
            .unwrap()
            .latest
            .take()
            .unwrap();
        engine
            .inner
            .shared
            .has_cached_frame
            .store(false, Ordering::Release);
        let shared = Arc::clone(&engine.inner.shared);
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                while !shared.capture_requested.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline, "request never arrived");
                    std::thread::yield_now();
                }
                shared.state.lock().unwrap().latest = Some(old);
                shared.ready.notify_all();
                std::thread::sleep(Duration::from_millis(80));
                shared.observed_sequence.store(2, Ordering::Release);
                let mut state = shared.state.lock().unwrap();
                let frame = state.latest.as_mut().unwrap();
                frame.sequence = 2;
                frame.bytes[0] = 88;
                drop(state);
                shared.ready.notify_all();
            });
            let mut copied = 0;
            let outcome = engine.copy_with_recovery(
                |_, _, bytes| {
                    copied = bytes[0];
                    Ok(())
                },
                |_| panic!("a pre-request crop must not end a fresh-frame wait"),
            );
            outcome.result.unwrap();
            assert!(outcome.replacement.is_none());
            assert_eq!(copied, 88);
        });
    }
    #[test]
    fn same_rectangle_from_a_new_uia_revision_is_not_old_frame_authorization() {
        let geometry = WindowGeometry::from_screen_rect(
            ScreenRect {
                x: 0,
                y: 0,
                width: 1600,
                height: 900,
            },
            1600,
            900,
        );
        let old = RemoteLayout {
            geometry,
            content_rect: PixelRect::new(100, 100, 1400, 700),
            revision: 1,
        };
        let next = RemoteLayout { revision: 2, ..old };
        assert!(require_matching_layout(Some(old), next).is_err());
        assert!(require_matching_layout(Some(next), next).is_ok());
    }
}
