use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, Result, anyhow};
use image::{ColorType, ImageEncoder, codecs::png::PngEncoder};

use super::{copy_rgba_to_clipboard, next_screenshot_path, windows_screenshots_folder};

/// Authorizes the side effects belonging to one capture generation.
///
/// Holding the mutex while invoking a side effect makes invalidation and the
/// beginning of that side effect a single serialized operation.
#[derive(Clone)]
pub struct CaptureAuthorization {
    valid: Arc<Mutex<bool>>,
}

impl Default for CaptureAuthorization {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureAuthorization {
    pub fn new() -> Self {
        Self {
            valid: Arc::new(Mutex::new(true)),
        }
    }

    pub fn invalidate(&self) {
        if let Ok(mut valid) = self.valid.lock() {
            *valid = false;
        }
    }

    pub fn with_current<T>(&self, write: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = self
            .valid
            .lock()
            .map_err(|_| anyhow!("撮影認可状態を確認できませんでした"))?;
        if !*_guard {
            return Err(anyhow!("撮影は失効しています"));
        }
        write()
    }
}

pub fn output_capture(
    authorization: &CaptureAuthorization,
    width: u32,
    height: u32,
    bytes: &[u8],
    save: bool,
) -> Result<Option<Result<PathBuf>>> {
    output_capture_with(
        authorization,
        save,
        || copy_rgba_to_clipboard(width, height, bytes),
        || encode_capture_png(width, height, bytes),
        |encoded| write_capture_png(&encoded),
    )
}

fn output_capture_with<T>(
    authorization: &CaptureAuthorization,
    save: bool,
    copy: impl FnOnce() -> Result<()>,
    encode: impl FnOnce() -> Result<T>,
    write: impl FnOnce(T) -> Result<PathBuf>,
) -> Result<Option<Result<PathBuf>>> {
    authorization.with_current(copy)?;
    if !save {
        return Ok(None);
    }
    let encoded = match encode() {
        Ok(encoded) => encoded,
        Err(error) => return Ok(Some(Err(error))),
    };
    Ok(Some(authorization.with_current(|| write(encoded))))
}

fn encode_capture_png(width: u32, height: u32, bytes: &[u8]) -> Result<Vec<u8>> {
    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("保存する画像のサイズが大きすぎます"))?;
    if bytes.len() != expected_len {
        return Err(anyhow!("保存する画像のバッファサイズが不正です"));
    }
    let mut encoded = Vec::new();
    PngEncoder::new(&mut encoded)
        .write_image(bytes, width, height, ColorType::Rgba8.into())
        .context("PNGのエンコードに失敗しました")?;
    Ok(encoded)
}

fn write_capture_png(encoded: &[u8]) -> Result<PathBuf> {
    let folder = windows_screenshots_folder()?;
    fs::create_dir_all(&folder).with_context(|| {
        format!(
            "Windowsのスクリーンショット保存先を作成できませんでした: {}",
            folder.display()
        )
    })?;
    let path = next_screenshot_path(&folder);
    fs::write(&path, encoded).with_context(|| {
        format!(
            "スクリーンショットを保存できませんでした: {}",
            path.display()
        )
    })?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{CaptureAuthorization, encode_capture_png, output_capture_with};
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn invalidation_prevents_output() {
        let auth = CaptureAuthorization::default();
        auth.invalidate();
        let copies = AtomicUsize::new(0);
        let saves = AtomicUsize::new(0);
        let result = output_capture_with(
            &auth,
            true,
            || {
                copies.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            || Ok(vec![1_u8]),
            |_| {
                saves.fetch_add(1, Ordering::SeqCst);
                Ok(std::path::PathBuf::new())
            },
        );
        assert!(result.is_err());
        assert_eq!(copies.load(Ordering::SeqCst), 0);
        assert_eq!(saves.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn default_authorization_is_valid() {
        assert!(
            CaptureAuthorization::default()
                .with_current(|| Ok(()))
                .is_ok()
        );
    }

    #[test]
    fn save_false_skips_encoding_and_saving() {
        let auth = CaptureAuthorization::new();
        let encoded = AtomicUsize::new(0);
        let saved = AtomicUsize::new(0);
        let result = output_capture_with(
            &auth,
            false,
            || Ok(()),
            || {
                encoded.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |_| {
                saved.fetch_add(1, Ordering::SeqCst);
                Ok(std::path::PathBuf::new())
            },
        )
        .unwrap();
        assert!(result.is_none());
        assert_eq!(encoded.load(Ordering::SeqCst), 0);
        assert_eq!(saved.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invalidation_between_copy_and_save_skips_only_save() {
        let auth = CaptureAuthorization::new();
        let saves = AtomicUsize::new(0);
        let result = output_capture_with(
            &auth,
            true,
            || Ok(()),
            || {
                auth.invalidate();
                Ok(vec![1_u8, 2, 3])
            },
            |_| {
                saves.fetch_add(1, Ordering::SeqCst);
                Ok(std::path::PathBuf::new())
            },
        )
        .unwrap();
        assert!(result.unwrap().is_err());
        assert_eq!(saves.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn save_uses_encoded_capture_data_after_external_change() {
        let auth = CaptureAuthorization::new();
        let mut external_clipboard = vec![9_u8];
        let captured = vec![1_u8, 2, 3, 4];
        let saved = Arc::new(std::sync::Mutex::new(Vec::new()));
        let saved_copy = saved.clone();
        let result = output_capture_with(
            &auth,
            true,
            || {
                external_clipboard = vec![8];
                Ok(())
            },
            || encode_capture_png(1, 1, &captured),
            move |encoded| {
                *saved_copy.lock().unwrap() =
                    image::load_from_memory(&encoded)?.to_rgba8().into_raw();
                Ok(std::path::PathBuf::new())
            },
        )
        .unwrap();
        assert!(result.unwrap().is_ok());
        assert_eq!(*saved.lock().unwrap(), captured);
        assert_eq!(external_clipboard, vec![8]);
    }

    #[test]
    fn png_failure_keeps_a_successful_clipboard_result() {
        let result = output_capture_with(
            &CaptureAuthorization::default(),
            true,
            || Ok(()),
            || Err(anyhow::anyhow!("PNG failed")),
            |_: Vec<u8>| panic!("failed encoding must not create a file"),
        )
        .unwrap();
        assert_eq!(result.unwrap().unwrap_err().to_string(), "PNG failed");
    }

    #[test]
    fn invalidation_waits_for_in_flight_output() {
        let auth = CaptureAuthorization::new();
        let entered = Arc::new(Barrier::new(2));
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_auth = auth.clone();
        let worker_entered = entered.clone();
        let worker_calls = calls.clone();
        let (release, wait_release) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_auth.with_current(|| {
                worker_calls.fetch_add(1, Ordering::SeqCst);
                worker_entered.wait();
                wait_release.recv().unwrap();
                Ok(())
            })
        });
        entered.wait();
        let invalidate_auth = auth.clone();
        let (attempted, wait_attempted) = std::sync::mpsc::channel();
        let invalidator = std::thread::spawn(move || {
            attempted.send(()).unwrap();
            invalidate_auth.invalidate();
        });
        wait_attempted.recv().unwrap();
        assert!(
            auth.valid.try_lock().is_err(),
            "active output must retain its write lock"
        );
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        invalidator.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            auth.with_current(|| -> anyhow::Result<()> {
                panic!("invalidated output must not run")
            })
            .is_err()
        );
    }
}
