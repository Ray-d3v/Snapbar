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
use xcap::Window;

use super::{
    CaptureAuthorization, CaptureReceipt, LocalMonitorCaptureTarget, ScreenRect,
    content_detector::PixelRect,
    flash::current_screen_rect,
    local_share::validate_local_monitor_target,
    output_capture,
    uia::{WindowGeometry, detect_content_rect},
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
    capture_requested: AtomicBool,
    observed_sequence: std::sync::atomic::AtomicU64,
    has_cached_frame: AtomicBool,
    state: Mutex<RuntimeState>,
    ready: Condvar,
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
    content_rect: Option<PixelRect>,
    last_error: Option<String>,
    requested_after_sequence: u64,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RemoteLayout {
    geometry: WindowGeometry,
    content_rect: PixelRect,
}

struct CaptureRequestGuard {
    shared: Arc<SharedState>,
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
    pub fn start_source(source: CaptureSource) -> Result<Self> {
        let shared = Arc::new(SharedState {
            source: source.clone(),
            capture_requested: AtomicBool::new(false),
            observed_sequence: std::sync::atomic::AtomicU64::new(0),
            has_cached_frame: AtomicBool::new(false),
            state: Mutex::new(RuntimeState::default()),
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
        self.inner.shared.has_cached_frame.load(Ordering::Acquire)
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
        // Bracket the requested frame with matching UIA observations. A UIA
        // read only after FrameArrived could describe a newer layout than the
        // pixels in that frame, even with identical window dimensions.
        let request_evidence = match authorization.with_current(|| Ok(())).and_then(|()| {
            let layout = self.confirm_request_layout()?;
            Ok((layout, performance_time_100ns()?))
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
        let (request_layout, requested_at_100ns) = request_evidence;
        let mut save_result = None;
        let mut outcome = self.copy_with_recovery_checked(
            |width, height, bytes| {
                save_result =
                    output_capture(authorization, width, height, bytes, save_to_screenshots)?;
                Ok(())
            },
            |source| authorization.with_current(|| Self::start_source(source)),
            |cached| {
                authorization.with_current(|| Ok(()))?;
                require_frame_after_request(cached.rendered_at_100ns, requested_at_100ns)?;
                self.validate_cached_layout(cached, request_layout)
            },
        );
        outcome.save_result = save_result;
        outcome
    }

    fn confirm_request_layout(&self) -> Result<Option<RemoteLayout>> {
        self.inner.shared.source.validate_remote_target()?;
        let Some(target_id) = self.inner.shared.source.remote_target_id() else {
            return Ok(None);
        };
        let (width, height) = {
            let state = self
                .inner
                .shared
                .state
                .lock()
                .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;
            let cached = state
                .latest
                .as_ref()
                .ok_or_else(|| anyhow!("共有コンテンツを準備中です"))?;
            (cached.source_width, cached.source_height)
        };
        detect_remote_layout(target_id, width, height).map(Some)
    }

    fn validate_cached_layout(
        &self,
        cached: &CachedFrame,
        request_layout: Option<RemoteLayout>,
    ) -> Result<()> {
        let Some(target_id) = self.inner.shared.source.remote_target_id() else {
            return Ok(());
        };
        let current = detect_remote_layout(target_id, cached.source_width, cached.source_height)?;
        require_matching_layout(request_layout, current)?;
        require_matching_layout(cached.remote_layout, current)
    }

    fn copy_with_recovery_checked(
        &self,
        mut copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
        restart: impl FnOnce(CaptureSource) -> Result<CaptureEngine>,
        mut validate: impl FnMut(&CachedFrame) -> Result<()>,
    ) -> CaptureOutcome {
        let first = self.copy_latest_since_checked(&mut copy, None, &mut validate);
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
        let result = new_engine.copy_latest_since_checked(copy, Some(0), validate);
        CaptureOutcome {
            result,
            replacement: Some(new_engine),
            save_result: None,
        }
    }

    fn copy_latest_since_checked(
        &self,
        mut copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
        baseline_override: Option<u64>,
        mut validate: impl FnMut(&CachedFrame) -> Result<()>,
    ) -> Result<CaptureReceipt> {
        let started_at = Instant::now();
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
        let mut state = self
            .inner
            .shared
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;
        let baseline_sequence = baseline_override
            .unwrap_or_else(|| self.inner.shared.observed_sequence.load(Ordering::Acquire));
        let had_frame = state.latest.is_some();
        state.requested_after_sequence = baseline_sequence;
        self.inner
            .shared
            .capture_requested
            .store(true, Ordering::Release);
        drop(state);
        let _request_guard = CaptureRequestGuard::new(Arc::clone(&self.inner.shared));

        let timeout = if !had_frame {
            READY_TIMEOUT
        } else if local_monitor {
            LOCAL_FRESH_FRAME_WAIT
        } else {
            FRESH_FRAME_WAIT
        };
        let deadline = Instant::now() + timeout;
        let mut state = self
            .inner
            .shared
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;

        loop {
            let has_fresh_frame = state
                .latest
                .as_ref()
                .is_some_and(|frame| frame.sequence > baseline_sequence);
            if state.latest.is_some() && (has_fresh_frame || !had_frame) {
                break;
            }

            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait_for = deadline.saturating_duration_since(now);
            let (next_state, wait_result) =
                self.inner
                    .shared
                    .ready
                    .wait_timeout(state, wait_for)
                    .map_err(|_| anyhow!("キャプチャ状態の待機に失敗しました"))?;
            state = next_state;
            if wait_result.timed_out() {
                break;
            }
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
        let observed_sequence = self
            .shared
            .observed_sequence
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let requested = self.shared.capture_requested.load(Ordering::Acquire);
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
                    state.latest = None;
                    state.content_rect = None;
                    self.shared.has_cached_frame.store(false, Ordering::Release);
                    state.last_error = Some(message);
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
                    state.latest = None;
                    state.content_rect = None;
                    state.last_error = Some(error.to_string());
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
        let layout = detect_remote_layout(target_id, source_width, source_height)?;
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
        state.last_error = None;
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
        Ok(())
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
    target_id: u32,
    source_width: u32,
    source_height: u32,
) -> Result<RemoteLayout> {
    let target = find_target_window(target_id)?;
    let geometry = WindowGeometry::from_window_dimensions(&target, source_width, source_height)?;
    let content_rect = detect_content_rect(target_id, geometry)?.ok_or_else(|| anyhow!(
        "Teamsの確定UIA共有要素を取得できませんでした。メニューから会議・共有を再検出してください"
    ))?;
    let current_geometry =
        WindowGeometry::from_window_dimensions(&target, source_width, source_height)?;
    if current_geometry != geometry {
        return Err(anyhow!("UIA確認中にTeamsの位置またはサイズが変わりました"));
    }
    Ok(RemoteLayout {
        geometry,
        content_rect,
    })
}

fn require_matching_layout(cached: Option<RemoteLayout>, current: RemoteLayout) -> Result<()> {
    if cached != Some(current) {
        return Err(anyhow!(
            "Teamsの共有範囲または配置が変わったため、もう一度撮影してください"
        ));
    }
    Ok(())
}

fn find_target_window(target_id: u32) -> Result<Window> {
    Window::all()
        .context("ウィンドウ一覧を再取得できませんでした")?
        .into_iter()
        .find(|window| window.id().ok() == Some(target_id))
        .ok_or_else(|| anyhow!("選択中のTeamsウィンドウが見つかりません"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::{
        Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_MINIMIZE,
            WS_OVERLAPPEDWINDOW,
        },
        core::w,
    };

    impl CaptureEngine {
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
                        last_error: None,
                        requested_after_sequence: 0,
                    }),
                    ready: Condvar::new(),
                }),
                control: Mutex::new(None),
            }),
        }
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
            geometry,
            content_rect: PixelRect::new(100, 100, 1400, 750),
        };
        let current = RemoteLayout {
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
            |_| Ok(replacement),
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
}
