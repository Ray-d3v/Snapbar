//! One MTA owns UIA identities, subscriptions, and discovery.
use super::{
    content_detector::PixelRect,
    uia::{ContentLocator, WindowGeometry},
};
use crate::shutdown::defer_cleanup;
use anyhow::{Result, anyhow};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{self, SyncSender},
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
struct Request {
    width: u32,
    height: u32,
    phase: String,
    queued: Instant,
    reply: SyncSender<std::result::Result<RemoteLayout, String>>,
}
#[derive(Default)]
struct Pending {
    capture: VecDeque<Request>,
    frame: VecDeque<Request>,
    structure: u64,
    properties: u64,
    stop: bool,
}
impl Pending {
    fn poll(&mut self, now: Instant, background_due: Instant) -> Work {
        if let Some(request) = self.next(now) {
            Work::Request(request)
        } else if now >= background_due {
            Work::Background
        } else {
            Work::Wait(background_due.duration_since(now))
        }
    }
    fn next(&mut self, now: Instant) -> Option<Request> {
        while let Some(request) = self.capture.pop_front().or_else(|| self.frame.pop_front()) {
            if now.saturating_duration_since(request.queued) < VERIFY_TIMEOUT {
                return Some(request);
            }
            crate::diagnostics::log(format_args!(
                "layout_request phase={} expired=true queue_wait_us={}",
                request.phase,
                now.saturating_duration_since(request.queued).as_micros()
            ));
            let _ = request
                .reply
                .try_send(Err("共有範囲の確認要求が期限切れになりました".into()));
        }
        None
    }
}
enum Work {
    Request(Request),
    Background,
    Wait(Duration),
}
#[derive(Default)]
pub(super) struct LayoutQueue {
    pending: Mutex<Pending>,
    wake: Condvar,
}
impl LayoutQueue {
    pub(super) fn changed(&self, structure: bool) {
        let mut pending = self.pending.lock().unwrap();
        let first = pending.structure == 0 && pending.properties == 0;
        if structure {
            pending.structure = pending.structure.saturating_add(1);
        } else {
            pending.properties = pending.properties.saturating_add(1);
        }
        // Notifications never occupy verification slots. Every event still
        // invalidates the revision immediately in the UIA callback.
        if first {
            self.wake.notify_one();
        }
    }
}
#[derive(Clone)]
pub(super) struct LayoutResolver(Arc<ResolverInner>);
struct ResolverInner {
    queue: Arc<LayoutQueue>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl LayoutResolver {
    pub(super) fn start(target: u32) -> Result<Self> {
        let queue = Arc::new(LayoutQueue::default());
        let worker_queue = queue.clone();
        let worker = thread::Builder::new()
            .name("snapbar-capture-layout".into())
            .spawn(move || run_resolver(target, worker_queue))?;
        Ok(Self(Arc::new(ResolverInner {
            queue,
            worker: Mutex::new(Some(worker)),
        })))
    }
    pub(super) fn verify(&self, width: u32, height: u32, phase: &str) -> Result<RemoteLayout> {
        let queued = Instant::now();
        let (reply, receiver) = mpsc::sync_channel(1);
        {
            let mut pending = self.0.queue.pending.lock().unwrap();
            if pending.stop {
                return Err(anyhow!("共有範囲の監視が停止しました"));
            }
            let requests = if phase == "frame" {
                &mut pending.frame
            } else {
                &mut pending.capture
            };
            if requests.len() >= 2 {
                crate::diagnostics::log(format_args!(
                    "layout_request phase={phase} queue_full=true"
                ));
                return Err(anyhow!(
                    "共有範囲を更新中です。少し待って再撮影してください"
                ));
            }
            requests.push_back(Request {
                width,
                height,
                phase: phase.into(),
                queued,
                reply,
            });
            self.0.queue.wake.notify_one();
        }
        receiver
            .recv_timeout(VERIFY_TIMEOUT.saturating_sub(queued.elapsed()))
            .map_err(|_| {
                crate::diagnostics::log(format_args!(
                    "layout_request phase={phase} timeout=true elapsed_us={}",
                    queued.elapsed().as_micros()
                ));
                anyhow!("Teamsの共有範囲の確認が応答していません")
            })?
            .map_err(|error| anyhow!(error))
    }
}
impl Drop for ResolverInner {
    fn drop(&mut self) {
        self.queue.pending.lock().unwrap().stop = true;
        self.queue.wake.notify_one();
        if let Some(worker) = self.worker.lock().ok().and_then(|mut value| value.take()) {
            defer_cleanup("snapbar-capture-layout-stop", move || {
                let _ = worker.join();
            });
        }
    }
}
fn run_resolver(target: u32, queue: Arc<LayoutQueue>) {
    let _physical = crate::dpi::PhysicalPixels::enter();
    let mut locator = ContentLocator::new(target, queue.clone());
    let mut dimensions = None;
    let mut last_background = Instant::now();
    loop {
        let request = {
            let mut pending = queue.pending.lock().unwrap();
            loop {
                if pending.stop {
                    return;
                }
                let background_due = locator
                    .as_ref()
                    .ok()
                    .and_then(ContentLocator::audit_deadline)
                    .unwrap_or(last_background + WATCHDOG);
                let remaining = match pending.poll(Instant::now(), background_due) {
                    Work::Request(request) => break Some(request),
                    Work::Wait(remaining) => remaining,
                    Work::Background => {
                        if pending.structure != 0 || pending.properties != 0 {
                            crate::diagnostics::log(format_args!(
                                "layout_events target={target} structure={} is_offscreen={}",
                                pending.structure, pending.properties
                            ));
                        }
                        pending.structure = 0;
                        pending.properties = 0;
                        break None;
                    }
                };
                pending = queue.wake.wait_timeout(pending, remaining).unwrap().0;
            }
        };
        if let Some(request) = request {
            let started = Instant::now();
            dimensions = Some((request.width, request.height));
            if locator.is_err() {
                locator = ContentLocator::new(target, queue.clone());
            }
            let result = match &mut locator {
                Ok(locator) => verify(target, request.width, request.height, locator, false),
                Err(error) => Err(anyhow!("{error:#}")),
            };
            crate::diagnostics::log(format_args!(
                "layout_request target={target} phase={} queue_wait_us={} verify_us={} success={} expired_after_verify={}",
                request.phase,
                started.duration_since(request.queued).as_micros(),
                started.elapsed().as_micros(),
                result.is_ok(),
                request.queued.elapsed() >= VERIFY_TIMEOUT
            ));
            let _ = request
                .reply
                .try_send(result.map_err(|error| format!("{error:#}")));
            // Check priority queues again before any background discovery.
        } else {
            if let (Some((width, height)), Ok(locator)) = (dimensions, locator.as_mut()) {
                let started = Instant::now();
                let result = WindowGeometry::from_hwnd_dimensions(target, width, height)
                    .and_then(|geometry| locator.background_step(geometry));
                crate::diagnostics::log(format_args!(
                    "layout_background target={target} verify_us={} success={} error={:?}",
                    started.elapsed().as_micros(),
                    result.is_ok(),
                    result.as_ref().err().map(|error| format!("{error:#}"))
                ));
            }
            last_background = Instant::now();
        }
    }
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
#[cfg(test)]
mod tests {
    use super::*;
    fn request(phase: &str, queued: Instant) -> Request {
        Request {
            width: 100,
            height: 100,
            phase: phase.into(),
            queued,
            reply: mpsc::sync_channel(1).0,
        }
    }
    #[test]
    fn event_storm_does_not_displace_capture_and_capture_precedes_frame() {
        let queue = LayoutQueue::default();
        for _ in 0..10_000 {
            queue.changed(true);
            queue.changed(false);
        }
        let mut pending = queue.pending.lock().unwrap();
        let now = Instant::now();
        pending.frame.push_back(request("frame", now));
        pending.capture.push_back(request("preflight", now));
        assert_eq!(pending.next(now).unwrap().phase, "preflight");
        assert_eq!(pending.next(now).unwrap().phase, "frame");
        assert!(pending.next(now).is_none());
        assert_eq!((pending.structure, pending.properties), (10_000, 10_000));
    }
    #[test]
    fn expired_requests_are_skipped_before_uia_work() {
        let now = Instant::now();
        let mut pending = Pending::default();
        pending
            .capture
            .push_back(request("expired", now - VERIFY_TIMEOUT));
        pending.frame.push_back(request("frame", now));
        assert_eq!(pending.next(now).unwrap().phase, "frame");
        assert!(pending.next(now).is_none());
    }
    #[test]
    fn audit_stability_wait_yields_to_requests_and_resumes_when_due() {
        let now = Instant::now();
        let resume = now + Duration::from_millis(35);
        let mut pending = Pending::default();
        assert!(
            matches!(pending.poll(now, resume), Work::Wait(wait) if wait == Duration::from_millis(35))
        );
        pending.capture.push_back(request("preflight", now));
        assert!(matches!(pending.poll(now, resume), Work::Request(_)));
        // Even a due second scan must yield to a request that arrived during
        // the first (uninterruptible) COM call.
        pending.capture.push_back(request("output", now));
        assert!(matches!(pending.poll(resume, resume), Work::Request(_)));
        assert!(matches!(pending.poll(resume, resume), Work::Background));
    }
}
