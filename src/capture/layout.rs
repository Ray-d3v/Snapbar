//! One MTA owns UIA identities, subscriptions, and discovery. No COM object or
//! cached UIA property value crosses to the capture/UI threads.
use super::{
    content_detector::PixelRect,
    uia::{ContentLocator, WindowGeometry},
};
use crate::shutdown::defer_cleanup;
use anyhow::{Result, anyhow};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const VERIFY_TIMEOUT: Duration = Duration::from_millis(1_200);
const WATCHDOG: Duration = Duration::from_millis(750);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RemoteLayout {
    pub(super) geometry: WindowGeometry,
    pub(super) content_rect: PixelRect,
    pub(super) revision: u64,
}

pub(super) enum Request {
    Verify {
        width: u32,
        height: u32,
        reply: SyncSender<std::result::Result<RemoteLayout, String>>,
    },
    Changed,
}

#[derive(Clone)]
pub(super) struct LayoutResolver(Arc<ResolverInner>);
struct ResolverInner {
    requests: SyncSender<Request>,
    stop: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl LayoutResolver {
    pub(super) fn start(target: u32) -> Result<Self> {
        // At most one frame callback and one serialized user request can wait.
        // Events coalesce through the locator's atomic revision even if full.
        let (requests, receiver) = mpsc::sync_channel(2);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let events = requests.clone();
        let worker = thread::Builder::new()
            .name("snapbar-capture-layout".into())
            .spawn(move || run_resolver(target, events, receiver, worker_stop))?;
        Ok(Self(Arc::new(ResolverInner {
            requests,
            stop,
            worker: Mutex::new(Some(worker)),
        })))
    }

    pub(super) fn verify(&self, width: u32, height: u32) -> Result<RemoteLayout> {
        let (reply, receiver) = mpsc::sync_channel(1);
        self.0
            .requests
            .try_send(Request::Verify {
                width,
                height,
                reply,
            })
            .map_err(|_| anyhow!("共有範囲を更新中です。少し待って再撮影してください"))?;
        receiver
            .recv_timeout(VERIFY_TIMEOUT)
            .map_err(|_| anyhow!("Teamsの共有範囲の確認が応答していません"))?
            .map_err(|error| anyhow!(error))
    }
}

impl Drop for ResolverInner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.requests.try_send(Request::Changed);
        if let Some(worker) = self.worker.lock().ok().and_then(|mut value| value.take()) {
            defer_cleanup("snapbar-capture-layout-stop", move || {
                let _ = worker.join();
            });
        }
    }
}

fn run_resolver(
    target: u32,
    events: SyncSender<Request>,
    receiver: Receiver<Request>,
    stop: Arc<AtomicBool>,
) {
    let _physical = crate::dpi::PhysicalPixels::enter();
    let mut locator = ContentLocator::new(target, events.clone());
    let mut dimensions = None;
    let mut last_background = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let request = receiver.recv_timeout(WATCHDOG);
        if stop.load(Ordering::Acquire) {
            break;
        }
        match request {
            Ok(Request::Verify {
                width,
                height,
                reply,
            }) => {
                dimensions = Some((width, height));
                // Retry initialization after an unavailable UIA provider, without
                // recreating a working client on every capture.
                if locator.is_err() {
                    locator = ContentLocator::new(target, events.clone());
                    if let Err(error) = &locator {
                        let _ = reply.try_send(Err(error.to_string()));
                        continue;
                    }
                }
                let result = verify(target, width, height, locator.as_mut().unwrap(), false);
                let _ = reply.try_send(result.map_err(|error| format!("{error:#}")));
                if last_background.elapsed() >= WATCHDOG {
                    let _ = verify(target, width, height, locator.as_mut().unwrap(), true);
                    last_background = Instant::now();
                }
            }
            Ok(Request::Changed) | Err(RecvTimeoutError::Timeout) => {
                if let (Some((width, height)), Ok(locator)) = (dimensions, locator.as_mut()) {
                    // Discovery and its stability delay normally happen here,
                    // including on static Teams surfaces that emit no WGC frame.
                    let _ = verify(target, width, height, locator, true);
                    last_background = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // UIA subscriptions and identities are released on their owning MTA.
}

fn verify(
    target: u32,
    width: u32,
    height: u32,
    locator: &mut ContentLocator,
    background: bool,
) -> Result<RemoteLayout> {
    let geometry = WindowGeometry::from_hwnd_dimensions(target, width, height)?;
    let (content_rect, revision) = locator.locate(geometry, background)?;
    let current = WindowGeometry::from_hwnd_dimensions(target, width, height)?;
    if current != geometry || !locator.is_current(revision) {
        return Err(anyhow!("共有範囲の確認中にTeamsの配置が変わりました"));
    }
    Ok(RemoteLayout {
        geometry,
        content_rect,
        revision,
    })
}
