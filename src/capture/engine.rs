use std::{
    ffi::c_void,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use windows_capture::{
    capture::{CaptureControl, Context, GraphicsCaptureApiHandler},
    frame::{DirtyRegion, Frame},
    graphics_capture_api::InternalCaptureControl,
    settings::{
        ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
        MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
    },
    window::Window as CaptureWindow,
};
use xcap::Window;

use super::{
    CaptureReceipt, ScreenRect,
    content_detector::PixelRect,
    copy_rgba_to_clipboard,
    flash::current_screen_rect,
    uia::{WindowGeometry, detect_content_rect},
};

const BACKUP_CACHE_INTERVAL: Duration = Duration::from_millis(750);
const FRESH_FRAME_WAIT: Duration = Duration::from_millis(45);
const DETECTION_RETRY_INTERVAL: Duration = Duration::from_millis(750);
const FULL_REDETECTION_INTERVAL: Duration = Duration::from_secs(8);
const READY_TIMEOUT: Duration = Duration::from_millis(1_200);

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
            let _ = control.stop();
        }
    }
}

pub(super) struct SharedState {
    target_id: u32,
    capture_requested: AtomicBool,
    state: Mutex<RuntimeState>,
    ready: Condvar,
}

#[derive(Default)]
struct RuntimeState {
    latest: Option<CachedFrame>,
    content_rect: Option<PixelRect>,
    last_error: Option<String>,
    frame_sequence: u64,
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
}

pub(super) struct FrameHandler {
    shared: Arc<SharedState>,
    last_cache_update: Option<Instant>,
    last_detection: Option<Instant>,
    last_source_size: Option<(u32, u32)>,
}

impl CaptureEngine {
    pub fn start(target_id: u32) -> Result<Self> {
        let shared = Arc::new(SharedState {
            target_id,
            capture_requested: AtomicBool::new(false),
            state: Mutex::new(RuntimeState::default()),
            ready: Condvar::new(),
        });
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
        let control = FrameHandler::start_free_threaded(settings)
            .map_err(|error| anyhow!("Teams共有画面のキャプチャを開始できませんでした: {error}"))?;

        Ok(Self {
            inner: Arc::new(EngineInner {
                shared,
                control: Mutex::new(Some(control)),
            }),
        })
    }

    pub fn is_ready(&self) -> bool {
        self.inner
            .shared
            .state
            .lock()
            .ok()
            .is_some_and(|state| state.latest.is_some())
    }

    pub fn copy_latest_to_clipboard(&self) -> Result<CaptureReceipt> {
        let started_at = Instant::now();
        let (baseline_sequence, had_frame) = self
            .inner
            .shared
            .state
            .lock()
            .map(|state| (state.frame_sequence, state.latest.is_some()))
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;
        self.inner
            .shared
            .capture_requested
            .store(true, Ordering::Release);

        let timeout = if had_frame {
            FRESH_FRAME_WAIT
        } else {
            READY_TIMEOUT
        };
        let deadline = Instant::now() + timeout;
        let mut state = self
            .inner
            .shared
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;

        loop {
            let has_fresh_frame = state.frame_sequence > baseline_sequence;
            if state.latest.is_some() && (has_fresh_frame || !had_frame) {
                break;
            }

            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let wait_for = deadline.saturating_duration_since(now);
            let (next_state, wait_result) = self
                .inner
                .shared
                .ready
                .wait_timeout(state, wait_for)
                .map_err(|_| anyhow!("キャプチャ状態の待機に失敗しました"))?;
            state = next_state;
            if wait_result.timed_out() {
                break;
            }
        }

        self.inner
            .shared
            .capture_requested
            .store(false, Ordering::Release);

        let cached = state.latest.as_ref().ok_or_else(|| {
            state.last_error.clone().map_or_else(
                || anyhow!("共有コンテンツを準備中です。少し待ってからもう一度撮影してください"),
                |message| anyhow!(message),
            )
        })?;
        copy_rgba_to_clipboard(cached.width, cached.height, cached.bytes.as_slice())?;
        let content_rect = cached.content_rect;
        let source_width = cached.source_width;
        let source_height = cached.source_height;
        let fallback_screen_rect = cached.fallback_screen_rect;
        let frame_age = cached.captured_at.elapsed();
        drop(state);

        let screen_rect = current_screen_rect(
            self.inner.shared.target_id,
            content_rect,
            source_width,
            source_height,
        )
        .unwrap_or(fallback_screen_rect);

        Ok(CaptureReceipt {
            screen_rect,
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
        let requested = self
            .shared
            .capture_requested
            .swap(false, Ordering::AcqRel);
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
            match self.detect_and_cache(frame, now) {
                Ok(()) => Ok(()),
                Err(error)
                    if can_reuse_confirmed_rect(
                        current_rect,
                        size_changed,
                        dirty_layout_change,
                    ) =>
                {
                    self.cache_crop(frame, current_rect.expect("checked above"), now, None)
                        .with_context(|| {
                            format!("UIA再検出後に確認済み範囲を再利用できませんでした: {error}")
                        })
                }
                Err(error) => Err(error),
            }
        } else if let Some(rect) = current_rect {
            self.cache_crop(frame, rect, now, None)
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
        }
        self.shared.ready.notify_all();
        Ok(())
    }
}

impl FrameHandler {
    fn detect_and_cache(&mut self, frame: &mut Frame, captured_at: Instant) -> Result<()> {
        let source_width = frame.width();
        let source_height = frame.height();
        let target = find_target_window(self.shared.target_id)?;
        let geometry =
            WindowGeometry::from_window_dimensions(&target, source_width, source_height)?;
        let content_rect = detect_content_rect(self.shared.target_id, geometry)?.ok_or_else(|| {
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
        )
    }

    fn cache_crop(
        &mut self,
        frame: &mut Frame,
        content_rect: PixelRect,
        captured_at: Instant,
        known_screen_rect: Option<ScreenRect>,
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
                current_screen_rect(
                    self.shared.target_id,
                    content_rect,
                    source_width,
                    source_height,
                )
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
        });
        state.content_rect = Some(content_rect);
        state.last_error = None;
        state.frame_sequence = state.frame_sequence.wrapping_add(1);
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
    use super::can_reuse_confirmed_rect;
    use crate::capture::content_detector::PixelRect;

    #[test]
    fn confirmed_rect_is_reused_only_for_transient_periodic_failures() {
        let rect = Some(PixelRect::new(12, 143, 2231, 1254));

        assert!(can_reuse_confirmed_rect(rect, false, false));
        assert!(!can_reuse_confirmed_rect(rect, true, false));
        assert!(!can_reuse_confirmed_rect(rect, false, true));
        assert!(!can_reuse_confirmed_rect(None, false, false));
    }
}
