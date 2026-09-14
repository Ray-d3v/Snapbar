use std::{
    ffi::{OsStr, c_void},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows::Win32::Foundation::HWND;

use crate::shutdown::defer_cleanup;

mod caption_diagnostics;

fn caption_diagnostics_requested(argument: &OsStr) -> bool {
    argument == "--caption-diagnostics"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ColorRequest {
    pub(super) target_id: u32,
    pub(super) caption_height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ColorSample {
    pub(super) request: ColorRequest,
    pub(super) material: Option<super::TitlebarMaterial>,
    pub(super) caption: Option<super::caption_anchor::CaptionObservation>,
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
            let mut probe = super::caption_anchor::CaptionProbe::new();
            let mut last_material = None;
            let mut last_read = Instant::now() - Duration::from_secs(1);
            let mut last_caption = None;
            let mut last_target = None;
            let sample = move |request: ColorRequest| {
                let hwnd = HWND(request.target_id as usize as *mut c_void);
                let caption = probe.measure(hwnd);
                if last_target != Some(request.target_id)
                    || caption != last_caption
                    || last_read.elapsed() >= Duration::from_millis(350)
                {
                    last_material = super::sample_titlebar_color(
                        hwnd,
                        request.caption_height,
                        &mut readback,
                        caption,
                    );
                    last_read = Instant::now();
                    last_caption = caption;
                    last_target = Some(request.target_id);
                }
                let material = last_material;
                (material, caption)
            };
            // Routine logging stays enabled. The much heavier diagnostic tree
            // is explicitly opt-in and never precedes publication of a sample.
            let trace_enabled = std::env::args_os().any(|arg| caption_diagnostics_requested(&arg));
            let mut trace = caption_diagnostics::CaptionTrace::default();
            let after_publish = move |request: ColorRequest, ready: bool| {
                if trace_enabled {
                    let hwnd = HWND(request.target_id as usize as *mut c_void);
                    trace.record_if_due(hwnd, ready);
                }
            };
            (sample, after_publish)
        })
    }

    #[cfg(test)]
    fn start_with_sample(
        wake_tx: SyncSender<()>,
        mut sample: impl FnMut(ColorRequest) -> Option<super::TitlebarMaterial> + Send + 'static,
    ) -> Option<Self> {
        Self::start_with_factory(wake_tx, move || {
            (move |request| (sample(request), None), |_, _| {})
        })
    }

    fn start_with_factory<F, After, Factory>(wake_tx: SyncSender<()>, factory: Factory) -> Option<Self>
    where
        Factory: FnOnce() -> (F, After) + Send + 'static,
        F: FnMut(
                ColorRequest,
            ) -> (
                Option<super::TitlebarMaterial>,
                Option<super::caption_anchor::CaptionObservation>,
            ) + 'static,
        After: FnMut(ColorRequest, bool) + 'static,
    {
        let (request_tx, request_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("snapbar-titlebar-color".to_string())
            .spawn(move || {
                let _physical = crate::dpi::PhysicalPixels::enter();
                let (mut sample, mut after_publish) = factory();
                while let Ok(request) = request_rx.recv() {
                    let (material, caption) = sample(request);
                    if result_tx
                        .try_send(ColorSample {
                            request,
                            material,
                            caption,
                        })
                        .is_ok()
                    {
                        let _ = wake_tx.try_send(());
                        // The follower can resize/redraw while an explicitly
                        // requested trace runs. The next sample always probes
                        // current geometry again; trace output is never reused.
                        after_publish(request, caption.is_some());
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
    fn detailed_diagnostics_require_the_exact_explicit_flag() {
        use std::ffi::OsStr;
        for argument in ["", "Snapbar.exe", "--demo-mode", "--caption-diagnostics=false"] {
            assert!(!super::caption_diagnostics_requested(OsStr::new(argument)));
        }
        assert!(super::caption_diagnostics_requested(OsStr::new("--caption-diagnostics")));
    }

    #[test]
    fn result_and_wake_are_published_before_a_blocked_optional_diagnostic() {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut sampler = ColorSampler::start_with_factory(wake_tx, move || {
            let mut first = true;
            (
                |_| (None, None),
                move |_, _| {
                    if first {
                        first = false;
                        entered_tx.send(()).unwrap();
                        release_rx.recv_timeout(TIMEOUT).unwrap();
                    }
                },
            )
        }).unwrap();
        assert!(sampler.request(request()));
        entered_rx.recv_timeout(TIMEOUT).unwrap();
        wake_rx.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(sampler.try_take_result().unwrap().request, request());
        // At most one follow-up request can queue while the diagnostic runs.
        assert!(sampler.request(request()));
        assert!(!sampler.request(request()));
        release_tx.send(()).unwrap();
        wake_rx.recv_timeout(TIMEOUT).unwrap();
        assert!(sampler.try_take_result().is_some());
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
                    separator_thickness: 1,
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
