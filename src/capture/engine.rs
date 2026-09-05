use std::fmt;
use std::{
    ffi::c_void,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{IsIconic, IsWindow},
};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::{DirtyRegion, Frame},
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
    CaptureReceipt, LocalMonitorCaptureTarget, ScreenRect,
    content_detector::PixelRect,
    copy_rgba_to_clipboard,
    flash::current_screen_rect,
    local_share::validate_local_monitor_target,
    uia::{WindowGeometry, detect_content_rect},
};
use crate::shutdown::defer_cleanup;

const BACKUP_CACHE_INTERVAL: Duration = Duration::from_millis(750);
const FRESH_FRAME_WAIT: Duration = Duration::from_millis(200);
const LOCAL_FRESH_FRAME_WAIT: Duration = Duration::from_millis(200);
const DETECTION_RETRY_INTERVAL: Duration = Duration::from_millis(750);
const FULL_REDETECTION_INTERVAL: Duration = Duration::from_secs(8);
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

    pub fn copy_latest_to_clipboard(&self) -> CaptureOutcome {
        self.copy_with_recovery(copy_rgba_to_clipboard, Self::start_source)
    }

    fn copy_with_recovery(
        &self,
        mut copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
        restart: impl FnOnce(CaptureSource) -> Result<CaptureEngine>,
    ) -> CaptureOutcome {
        let first = self.copy_latest_with(&mut copy);
        if first
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<FrameUnavailable>())
            .is_none()
        {
            return CaptureOutcome {
                result: first,
                replacement: None,
            };
        }

        let source = self.inner.shared.source.clone();
        if let Err(error) = source.validate_remote_target() {
            return CaptureOutcome {
                result: Err(error),
                replacement: None,
            };
        }
        let new_engine = match restart(source) {
            Ok(engine) => engine,
            Err(error) => {
                return CaptureOutcome {
                    result: Err(error),
                    replacement: None,
                };
            }
        };
        let result = new_engine.copy_latest_since(copy, Some(0));
        CaptureOutcome {
            result,
            replacement: Some(new_engine),
        }
    }

    fn copy_latest_with(
        &self,
        mut copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
    ) -> Result<CaptureReceipt> {
        self.copy_latest_since(&mut copy, None)
    }

    fn copy_latest_since(
        &self,
        mut copy: impl FnMut(u32, u32, &[u8]) -> Result<()>,
        baseline_override: Option<u64>,
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

        let observed_sequence = self.inner.shared.observed_sequence.load(Ordering::Acquire);
        let cached_sequence = state.latest.as_ref().map_or(0, |frame| frame.sequence);
        if local_monitor && cached_sequence <= baseline_sequence {
            return Err(anyhow::Error::new(FrameUnavailable(
                "自分の共有画面の新しいフレームを取得できませんでした",
            )));
        }
        if !local_monitor
            && !remote_cache_is_usable(baseline_sequence, observed_sequence, cached_sequence)
        {
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
        let (current_rect, has_latest) = self
            .shared
            .state
            .lock()
            .map(|state| (state.content_rect, state.latest.is_some()))
            .unwrap_or((None, false));
        let size_changed = self.last_source_size != Some(source_size);
        if size_changed {
            self.last_source_size = Some(source_size);
        }
        let periodic_redetect = current_rect.is_some()
            && self
                .last_detection
                .is_none_or(|last| now.duration_since(last) >= FULL_REDETECTION_INTERVAL);
        let missing_rect_retry = current_rect.is_none()
            && (requested
                || self
                    .last_detection
                    .is_none_or(|last| now.duration_since(last) >= DETECTION_RETRY_INTERVAL));
        let dirty_layout_change = current_rect.is_some_and(|rect| {
            self.last_detection
                .is_none_or(|last| now.duration_since(last) >= DETECTION_RETRY_INTERVAL)
                && dirty_regions_suggest_layout_change(frame, rect)
        });
        let needs_detection =
            size_changed || periodic_redetect || missing_rect_retry || dirty_layout_change;
        let needs_cache = requested
            || !has_latest
            || self
                .last_cache_update
                .is_none_or(|last| now.duration_since(last) >= BACKUP_CACHE_INTERVAL);

        if current_rect.is_none() && !needs_detection {
            return Ok(());
        }
        if !needs_detection && !needs_cache {
            return Ok(());
        }

        let result = if needs_detection {
            self.last_detection = Some(now);
            match self.detect_and_cache(frame, now, observed_sequence) {
                Ok(()) => Ok(()),
                Err(error)
                    if can_reuse_confirmed_rect(
                        current_rect,
                        size_changed,
                        dirty_layout_change,
                    ) =>
                {
                    self.cache_crop(
                        frame,
                        current_rect.expect("checked above"),
                        now,
                        None,
                        observed_sequence,
                    )
                    .with_context(|| {
                        format!("UIA再検出後に確認済み範囲を再利用できませんでした: {error}")
                    })
                }
                Err(error) => Err(error),
            }
        } else if let Some(rect) = current_rect {
            self.cache_crop(frame, rect, now, None, observed_sequence)
        } else {
            Err(anyhow!("共有コンテンツ領域がまだ特定されていません"))
        };

        match result {
            Ok(()) => {
                self.last_cache_update = Some(now);
            }
            Err(error) => {
                let message = error.to_string();
                if let Ok(mut state) = self.shared.state.lock() {
                    if needs_detection {
                        state.latest = None;
                        state.content_rect = None;
                        self.shared.has_cached_frame.store(false, Ordering::Release);
                    }
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
                Some(target.screen_rect),
                observed_sequence,
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
        let target = find_target_window(target_id)?;
        let geometry =
            WindowGeometry::from_window_dimensions(&target, source_width, source_height)?;
        let content_rect = detect_content_rect(target_id, geometry)?.ok_or_else(|| {
            anyhow!(
                "Teamsの確定UIA共有要素を取得できませんでした。精度優先のため画像推定は自動採用しません。メニューから会議・共有を再検出してください"
            )
        })?;
        let fallback_screen_rect = geometry
            .map_pixel_rect_to_screen(content_rect)
            .ok_or_else(|| anyhow!("共有コンテンツの画面座標を計算できませんでした"))?;
        self.cache_crop(
            frame,
            content_rect,
            captured_at,
            Some(fallback_screen_rect),
            observed_sequence,
        )
    }

    fn cache_crop(
        &mut self,
        frame: &mut Frame,
        content_rect: PixelRect,
        captured_at: Instant,
        known_screen_rect: Option<ScreenRect>,
        sequence: u64,
    ) -> Result<()> {
        let source_width = frame.width();
        let source_height = frame.height();
        if content_rect.x.saturating_add(content_rect.width) > source_width
            || content_rect.y.saturating_add(content_rect.height) > source_height
        {
            return Err(anyhow!("Teamsのレイアウト変更を検出しました"));
        }

        let fallback_screen_rect = known_screen_rect
            .or_else(|| {
                self.shared.source.remote_target_id().and_then(|target_id| {
                    current_screen_rect(target_id, content_rect, source_width, source_height)
                })
            })
            .or_else(|| {
                self.shared.state.lock().ok().and_then(|state| {
                    state
                        .latest
                        .as_ref()
                        .map(|frame| frame.fallback_screen_rect)
                })
            })
            .ok_or_else(|| anyhow!("共有コンテンツの画面座標を計算できませんでした"))?;

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

fn can_reuse_confirmed_rect(
    current_rect: Option<PixelRect>,
    size_changed: bool,
    dirty_layout_change: bool,
) -> bool {
    current_rect.is_some() && !size_changed && !dirty_layout_change
}

fn remote_cache_is_usable(
    baseline_sequence: u64,
    observed_sequence: u64,
    cached_sequence: u64,
) -> bool {
    cached_sequence > baseline_sequence || cached_sequence == observed_sequence
}

fn find_target_window(target_id: u32) -> Result<Window> {
    Window::all()
        .context("ウィンドウ一覧を再取得できませんでした")?
        .into_iter()
        .find(|window| window.id().ok() == Some(target_id))
        .ok_or_else(|| anyhow!("選択中のTeamsウィンドウが見つかりません"))
}

fn dirty_regions_suggest_layout_change(frame: &Frame, content_rect: PixelRect) -> bool {
    let Ok(regions) = frame.dirty_regions() else {
        return false;
    };
    let frame_width = frame.width();
    let frame_height = frame.height();
    let frame_area = u64::from(frame_width) * u64::from(frame_height);
    if frame_area == 0 {
        return false;
    }

    regions.iter().any(|region| {
        let Some(region_rect) = dirty_region_to_rect(region, frame_width, frame_height) else {
            return false;
        };
        let region_area = u64::from(region_rect.width) * u64::from(region_rect.height);
        let area_ratio = region_area as f64 / frame_area as f64;
        if !(0.01..=0.45).contains(&area_ratio) {
            return false;
        }
        if overlap_ratio(region_rect, content_rect) >= 0.35 {
            return false;
        }

        let region_right = region_rect.x.saturating_add(region_rect.width);
        let region_bottom = region_rect.y.saturating_add(region_rect.height);
        let content_right = content_rect.x.saturating_add(content_rect.width);
        let content_bottom = content_rect.y.saturating_add(content_rect.height);
        let boundary_tolerance = 32;
        let near_vertical_boundary = region_rect.x.abs_diff(content_right) <= boundary_tolerance
            || region_right.abs_diff(content_rect.x) <= boundary_tolerance;
        let near_horizontal_boundary = region_rect.y.abs_diff(content_bottom) <= boundary_tolerance
            || region_bottom.abs_diff(content_rect.y) <= boundary_tolerance;
        let tall_band = region_rect.height.saturating_mul(100) >= frame_height.saturating_mul(55);
        let wide_band = region_rect.width.saturating_mul(100) >= frame_width.saturating_mul(55);

        (near_vertical_boundary && tall_band) || (near_horizontal_boundary && wide_band)
    })
}

fn dirty_region_to_rect(
    region: &DirtyRegion,
    frame_width: u32,
    frame_height: u32,
) -> Option<PixelRect> {
    if region.width <= 0 || region.height <= 0 {
        return None;
    }
    let x = region.x.max(0) as u32;
    let y = region.y.max(0) as u32;
    let right =
        (i64::from(region.x) + i64::from(region.width)).clamp(0, i64::from(frame_width)) as u32;
    let bottom =
        (i64::from(region.y) + i64::from(region.height)).clamp(0, i64::from(frame_height)) as u32;
    (right > x && bottom > y).then_some(PixelRect::new(x, y, right - x, bottom - y))
}

fn overlap_ratio(left: PixelRect, right: PixelRect) -> f64 {
    let intersection_left = left.x.max(right.x);
    let intersection_top = left.y.max(right.y);
    let intersection_right = left
        .x
        .saturating_add(left.width)
        .min(right.x.saturating_add(right.width));
    let intersection_bottom = left
        .y
        .saturating_add(left.height)
        .min(right.y.saturating_add(right.height));
    if intersection_right <= intersection_left || intersection_bottom <= intersection_top {
        return 0.0;
    }
    let intersection_area = u64::from(intersection_right - intersection_left)
        * u64::from(intersection_bottom - intersection_top);
    let left_area = u64::from(left.width) * u64::from(left.height);
    if left_area == 0 {
        0.0
    } else {
        intersection_area as f64 / left_area as f64
    }
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
    fn confirmed_rect_is_reused_only_for_transient_periodic_failures() {
        let rect = Some(PixelRect::new(12, 143, 2231, 1254));

        assert!(can_reuse_confirmed_rect(rect, false, false));
        assert!(!can_reuse_confirmed_rect(rect, true, false));
        assert!(!can_reuse_confirmed_rect(rect, false, true));
        assert!(!can_reuse_confirmed_rect(None, false, false));
    }

    #[test]
    fn remote_stale_cache_is_allowed_when_no_frame_was_observed() {
        assert!(remote_cache_is_usable(10, 10, 10));
        assert!(!remote_cache_is_usable(10, 10, 7));
    }

    #[test]
    fn remote_cache_must_match_the_latest_observed_frame() {
        assert!(remote_cache_is_usable(10, 12, 12));
        assert!(!remote_cache_is_usable(10, 12, 10));
        assert!(!remote_cache_is_usable(10, 12, 0));
    }

    #[test]
    fn remote_cache_accepts_a_new_cache_even_if_a_later_frame_was_not_cached() {
        assert!(!remote_cache_is_usable(10, 10, 1));
        assert!(remote_cache_is_usable(10, 12, 11));
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
