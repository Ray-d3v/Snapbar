use std::{
    ffi::c_void,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread::{self, JoinHandle},
};

use windows::Win32::Foundation::HWND;

use crate::shutdown::defer_cleanup;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ColorRequest {
    pub(super) target_id: u32,
    pub(super) caption_height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ColorSample {
    pub(super) request: ColorRequest,
    pub(super) material: Option<super::TitlebarMaterial>,
}

pub(super) struct ColorSampler {
    request_tx: Option<SyncSender<ColorRequest>>,
    result_rx: Receiver<ColorSample>,
    worker: Option<JoinHandle<()>>,
    busy: bool,
}

impl ColorSampler {
    pub(super) fn start(wake_tx: SyncSender<()>) -> Option<Self> {
        Self::start_with_factory(wake_tx, || {
            let mut readback = super::caption_readback::CaptionReadback::default();
            move |request: ColorRequest| {
                super::sample_titlebar_color(
                    HWND(request.target_id as usize as *mut c_void),
                    request.caption_height,
                    &mut readback,
                )
            }
        })
    }

    #[cfg(test)]
    fn start_with_sample(
        wake_tx: SyncSender<()>,
        sample: impl FnMut(ColorRequest) -> Option<super::TitlebarMaterial> + Send + 'static,
    ) -> Option<Self> {
        Self::start_with_factory(wake_tx, move || sample)
    }

    fn start_with_factory<F, Factory>(wake_tx: SyncSender<()>, factory: Factory) -> Option<Self>
    where
        Factory: FnOnce() -> F + Send + 'static,
        F: FnMut(ColorRequest) -> Option<super::TitlebarMaterial> + 'static,
    {
        let (request_tx, request_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("snapbar-titlebar-color".to_string())
            .spawn(move || {
                let mut sample = factory();
                while let Ok(request) = request_rx.recv() {
                    let material = sample(request);
                    if result_tx
                        .try_send(ColorSample { request, material })
                        .is_ok()
                    {
                        let _ = wake_tx.try_send(());
                    }
                }
            })
            .ok()?;

        Some(Self {
            request_tx: Some(request_tx),
            result_rx,
            worker: Some(worker),
            busy: false,
        })
    }

    pub(super) fn request(&mut self, request: ColorRequest) -> bool {
        if self.busy {
            return false;
        }
        let Some(request_tx) = self.request_tx.as_ref() else {
            return false;
        };
        match request_tx.try_send(request) {
            Ok(()) => {
                self.busy = true;
                true
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
        }
    }

    pub(super) fn try_take_result(&mut self) -> Option<ColorSample> {
        match self.result_rx.try_recv() {
            Ok(result) => {
                self.busy = false;
                Some(result)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

impl Drop for ColorSampler {
    fn drop(&mut self) {
        self.request_tx.take();
        if let Some(worker) = self.worker.take() {
            defer_cleanup("snapbar-titlebar-color-stop", move || {
                let _ = worker.join();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, thread, time::Duration};

    use super::{ColorRequest, ColorSampler};

    const TIMEOUT: Duration = Duration::from_secs(2);

    fn request() -> ColorRequest {
        ColorRequest {
            target_id: 7,
            caption_height: 32,
        }
    }

    #[test]
    fn blocked_color_read_keeps_its_caller_responsive_without_queueing() {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (responsive_tx, responsive_rx) = mpsc::sync_channel(1);
        let next_request = ColorRequest {
            target_id: 19,
            caption_height: 45,
        };
        let caller = thread::spawn(move || {
            let mut sampler = ColorSampler::start_with_sample(wake_tx, move |request| {
                entered_tx.send(request).expect("sample should start");
                release_rx.recv_timeout(TIMEOUT).expect("release sample");
                Some(super::super::TitlebarMaterial {
                    surface: request.target_id,
                    separator: request.target_id,
                    separator_offset: 0,
                })
            })
            .expect("worker should start");

            assert!(sampler.request(request()));
            for _ in 0..100 {
                assert!(!sampler.request(next_request));
                assert!(sampler.try_take_result().is_none());
            }
            responsive_tx
                .send(())
                .expect("caller should remain responsive");
            wake_rx.recv_timeout(TIMEOUT).expect("first result wake");
            let first = sampler.try_take_result().expect("first result");
            assert_eq!(first.request, request());
            assert_eq!(first.material.map(|value| value.surface), Some(7));

            assert!(sampler.request(next_request));
            wake_rx.recv_timeout(TIMEOUT).expect("second result wake");
            let second = sampler.try_take_result().expect("second result");
            assert_eq!(second.request, next_request);
            assert_eq!(second.material.map(|value| value.surface), Some(19));
        });

        assert_eq!(entered_rx.recv_timeout(TIMEOUT).unwrap(), request());
        // Do not release the slow read until request/receive calls have returned.
        responsive_rx
            .recv_timeout(TIMEOUT)
            .expect("caller must not wait for sampling");
        release_tx.send(()).unwrap();
        assert_eq!(entered_rx.recv_timeout(TIMEOUT).unwrap(), next_request);
        release_tx.send(()).unwrap();
        caller.join().expect("caller should finish");
    }

    #[test]
    fn dropping_sampler_does_not_wait_for_an_inflight_read() {
        let (wake_tx, _wake_rx) = mpsc::sync_channel(1);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let (returned_tx, returned_rx) = mpsc::sync_channel(1);
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        let caller = thread::spawn(move || {
            let mut sampler = ColorSampler::start_with_sample(wake_tx, move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(TIMEOUT).expect("release sample");
                finished_tx.send(()).unwrap();
                None
            })
            .expect("worker should start");
            assert!(sampler.request(request()));
            entered_rx
                .recv_timeout(TIMEOUT)
                .expect("sample should start");
            drop(sampler);
            returned_tx.send(()).unwrap();
        });

        returned_rx
            .recv_timeout(TIMEOUT)
            .expect("drop must not wait for sampling");
        release_tx.send(()).unwrap();
        finished_rx
            .recv_timeout(TIMEOUT)
            .expect("sample should finish");
        caller.join().expect("caller should finish");
    }
}
