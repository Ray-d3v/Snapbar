use std::{
    ffi::c_void,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use image::{RgbaImage, imageops};
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
    content_detector::{PixelRect, select_content_rect},
    copy_rgba_to_clipboard,
    flash::current_screen_rect,
    uia::{WindowGeometry, detect_content_candidates},
};

const FRAME_CACHE_INTERVAL: Duration = Duration::from_millis(50);
const FULL_REDETECTION_INTERVAL: Duration = Duration::from_secs(3);
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
            let _ = thread::Builder::new()
                .name("snapbar-capture-stop".to_string())
                .spawn(move || {
                    let _ = control.stop();
                });
        }
    }
}

struct SharedState {
    target_id: u32,
    state: Mutex<RuntimeState>,
    ready: Condvar,
}

#[derive(Default)]
struct RuntimeState {
    latest: Option<CachedFrame>,
    content_rect: Option<PixelRect>,
    last_error: Option<String>,
}

#[derive(Clone)]
struct CachedFrame {
    width: u32,
    height: u32,
    bytes: Arc<Vec<u8>>,
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
    scratch: Vec<u8>,
}

impl CaptureEngine {
    pub fn start(target_id: u32) -> Result<Self> {
        let shared = Arc::new(SharedState {
            target_id,
            state: Mutex::new(RuntimeState::default()),
            ready: Condvar::new(),
        });
        let target = CaptureWindow::from_raw_hwnd(target_id as usize as *mut c_void);
        let settings = Settings::new(
            target,
            CursorCaptureSettings::WithoutCursor,
            DrawBorderSettings::Default,
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

    pub fn copy_latest_to_clipboard(&self) -> Result<CaptureReceipt> {
        let started_at = Instant::now();
        let cached = self.wait_for_cached_frame()?;
        copy_rgba_to_clipboard(cached.width, cached.height, &cached.bytes)?;

        let screen_rect = current_screen_rect(
            self.inner.shared.target_id,
            cached.content_rect,
            cached.source_width,
            cached.source_height,
        )
        .unwrap_or(cached.fallback_screen_rect);

        Ok(CaptureReceipt {
            screen_rect,
            latency: started_at.elapsed(),
            frame_age: cached.captured_at.elapsed(),
        })
    }

    fn wait_for_cached_frame(&self) -> Result<CachedFrame> {
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut state = self
            .inner
            .shared
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を取得できませんでした"))?;

        loop {
            if let Some(frame) = state.latest.clone() {
                return Ok(frame);
            }
            if let Some(error) = state.last_error.clone() {
                return Err(anyhow!(error));
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(anyhow!(
                    "共有コンテンツを準備中です。少し待ってからもう一度撮影してください"
                ));
            }
            let timeout = deadline.saturating_duration_since(now);
            let (next_state, wait_result) = self
                .inner
                .shared
                .ready
                .wait_timeout(state, timeout)
                .map_err(|_| anyhow!("キャプチャ状態の待機に失敗しました"))?;
            state = next_state;
            if wait_result.timed_out() && state.latest.is_none() {
                return Err(anyhow!(
                    "共有コンテンツを準備中です。少し待ってからもう一度撮影してください"
                ));
            }
        }
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
            scratch: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _: InternalCaptureControl,
    ) -> std::result::Result<(), Self::Error> {
        let now = Instant::now();
        let source_size = (frame.width(), frame.height());
        let current_rect = self
            .shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.content_rect);
        let size_changed = self.last_source_size != Some(source_size);
        let periodic_redetect = self
            .last_detection
            .is_none_or(|last| now.duration_since(last) >= FULL_REDETECTION_INTERVAL);
        let dirty_layout_change =
            current_rect.is_some_and(|rect| dirty_regions_suggest_layout_change(frame, rect));
        let needs_detection =
            size_changed || periodic_redetect || dirty_layout_change || current_rect.is_none();

        if !needs_detection
            && self
                .last_cache_update
                .is_some_and(|last| now.duration_since(last) < FRAME_CACHE_INTERVAL)
        {
            return Ok(());
        }

        let result = if needs_detection {
            self.detect_and_cache(frame, now)
        } else if let Some(rect) = current_rect {
            self.cache_crop(frame, rect, now)
        } else {
            Err(anyhow!("共有コンテンツ領域がまだ特定されていません"))
        };

        match result {
            Ok(()) => {
                self.last_cache_update = Some(now);
                self.last_source_size = Some(source_size);
                if needs_detection {
                    self.last_detection = Some(now);
                }
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
        let image = frame_to_image(frame, &mut self.scratch)?;
        let target = find_target_window(self.shared.target_id)?;
        let geometry = WindowGeometry::from_window(&target, &image)?;
        let semantic_candidates = detect_content_candidates(geometry).unwrap_or_default();
        let content_rect = select_content_rect(&image, &semantic_candidates, true).ok_or_else(|| {
            anyhow!(
                "共有コンテンツ領域を安全に特定できなかったため、ウィンドウ全体はコピーしませんでした"
            )
        })?;
        let fallback_screen_rect = geometry
            .map_pixel_rect_to_screen(content_rect)
            .ok_or_else(|| anyhow!("共有コンテンツの画面座標を計算できませんでした"))?;
        let cropped = imageops::crop_imm(
            &image,
            content_rect.x,
            content_rect.y,
            content_rect.width,
            content_rect.height,
        )
        .to_image();
        self.publish_frame(
            content_rect,
            fallback_screen_rect,
            image.width(),
            image.height(),
            cropped.into_raw(),
            captured_at,
        )
    }

    fn cache_crop(
        &mut self,
        frame: &mut Frame,
        content_rect: PixelRect,
        captured_at: Instant,
    ) -> Result<()> {
        if content_rect.x.saturating_add(content_rect.width) > frame.width()
            || content_rect.y.saturating_add(content_rect.height) > frame.height()
        {
            return Err(anyhow!("Teamsのレイアウト変更を検出しました"));
        }

        let fallback_screen_rect = current_screen_rect(
            self.shared.target_id,
            content_rect,
            frame.width(),
            frame.height(),
        )
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
        let bytes = buffer.as_nopadding_buffer(&mut self.scratch).to_vec();
        self.publish_frame(
            content_rect,
            fallback_screen_rect,
            frame.width(),
            frame.height(),
            bytes,
            captured_at,
        )
    }

    fn publish_frame(
        &self,
        content_rect: PixelRect,
        fallback_screen_rect: ScreenRect,
        source_width: u32,
        source_height: u32,
        bytes: Vec<u8>,
        captured_at: Instant,
    ) -> Result<()> {
        let expected_len = content_rect.width as usize * content_rect.height as usize * 4;
        if bytes.len() != expected_len {
            return Err(anyhow!("共有画面フレームのバッファサイズが不正です"));
        }

        let cached = CachedFrame {
            width: content_rect.width,
            height: content_rect.height,
            bytes: Arc::new(bytes),
            captured_at,
            content_rect,
            fallback_screen_rect,
            source_width,
            source_height,
        };
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| anyhow!("キャプチャ状態を更新できませんでした"))?;
        state.latest = Some(cached);
        state.content_rect = Some(content_rect);
        state.last_error = None;
        drop(state);
        self.shared.ready.notify_all();
        Ok(())
    }
}

fn frame_to_image(frame: &mut Frame, scratch: &mut Vec<u8>) -> Result<RgbaImage> {
    let width = frame.width();
    let height = frame.height();
    let mut buffer = frame
        .buffer()
        .context("Teamsウィンドウの最新フレームを取得できませんでした")?;
    let bytes = buffer.as_nopadding_buffer(scratch).to_vec();
    RgbaImage::from_raw(width, height, bytes)
        .ok_or_else(|| anyhow!("Teamsウィンドウの画像バッファを構築できませんでした"))
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
    let frame_area = u64::from(frame.width()) * u64::from(frame.height());
    if frame_area == 0 {
        return false;
    }

    regions.iter().any(|region| {
        let Some(region_rect) = dirty_region_to_rect(region, frame.width(), frame.height()) else {
            return false;
        };
        let region_area = u64::from(region_rect.width) * u64::from(region_rect.height);
        let area_ratio = region_area as f64 / frame_area as f64;
        if !(0.02..=0.35).contains(&area_ratio) {
            return false;
        }
        overlap_ratio(region_rect, content_rect) < 0.35
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
    let left_edge = left.x.max(right.x);
    let top_edge = left.y.max(right.y);
    let right_edge = left
        .x
        .saturating_add(left.width)
        .min(right.x.saturating_add(right.width));
    let bottom_edge = left
        .y
        .saturating_add(left.height)
        .min(right.y.saturating_add(right.height));
    if right_edge <= left_edge || bottom_edge <= top_edge {
        return 0.0;
    }
    let intersection = u64::from(right_edge - left_edge) * u64::from(bottom_edge - top_edge);
    let left_area = u64::from(left.width) * u64::from(left.height);
    if left_area == 0 {
        0.0
    } else {
        intersection as f64 / left_area as f64
    }
}

#[cfg(test)]
mod tests {
    use super::{dirty_region_to_rect, overlap_ratio};
    use crate::capture::content_detector::PixelRect;
    use windows_capture::frame::DirtyRegion;

    #[test]
    fn dirty_region_is_clipped_to_frame() {
        let dirty = DirtyRegion {
            x: -20,
            y: 10,
            width: 80,
            height: 40,
        };
        assert_eq!(
            dirty_region_to_rect(&dirty, 100, 100),
            Some(PixelRect::new(0, 10, 60, 40))
        );
    }

    #[test]
    fn overlap_ratio_uses_dirty_region_area() {
        let dirty = PixelRect::new(0, 0, 100, 100);
        let content = PixelRect::new(50, 0, 100, 100);
        assert!((overlap_ratio(dirty, content) - 0.5).abs() < f64::EPSILON);
    }
}
