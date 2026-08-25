use std::{
    ffi::c_void,
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use uiautomation::UIAutomation;
use uiautomation::types::{ControlType, Handle, Point, TreeScope};
use windows::Win32::{
    Foundation::{BOOL, HWND, LPARAM},
    System::Threading::GetCurrentThreadId,
    UI::{
        Accessibility::{
            EVENT_OBJECT_CREATE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, HWINEVENTHOOK, SetWinEventHook,
            UnhookWinEvent, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
        },
        WindowsAndMessaging::{
            EnumChildWindows, GetClassNameW, GetMessageW, IsIconic, IsWindow, IsWindowVisible, MSG,
            PostThreadMessageW, WM_QUIT,
        },
    },
};
use xcap::Window;

use crate::capture::CaptureTarget;

const WATCHDOG_INTERVAL: Duration = Duration::from_millis(700);
const PROVIDER_WARMUP_DELAY: Duration = Duration::from_millis(40);
const ENTRY_STABLE_FOR: Duration = Duration::from_millis(600);
const ENTRY_FALLBACK_STABLE_FOR: Duration = Duration::from_millis(1_100);
const EXIT_STABLE_FOR: Duration = Duration::from_millis(1_400);
const REQUIRED_ENTRY_SCANS: u8 = 2;
const REQUIRED_EXIT_SCANS: u8 = 2;

static WIN_EVENT_SIGNAL: OnceLock<Weak<ScanSignal>> = OnceLock::new();

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeetingSnapshot {
    pub generation: u64,
    pub target: Option<CaptureTarget>,
    pub minimized: bool,
    pub shared_content_hint: bool,
}

pub struct MeetingMonitor {
    snapshot: Arc<Mutex<MeetingSnapshot>>,
    signal: Arc<ScanSignal>,
    stop: Arc<AtomicBool>,
    hook_thread_id: Arc<AtomicU32>,
    workers: Vec<JoinHandle<()>>,
}

impl MeetingMonitor {
    pub fn start() -> Self {
        let snapshot = Arc::new(Mutex::new(MeetingSnapshot::default()));
        let signal = Arc::new(ScanSignal::default());
        let stop = Arc::new(AtomicBool::new(false));
        let hook_thread_id = Arc::new(AtomicU32::new(0));
        let mut workers = Vec::new();

        let _ = WIN_EVENT_SIGNAL.set(Arc::downgrade(&signal));

        {
            let signal = Arc::clone(&signal);
            let stop = Arc::clone(&stop);
            let hook_thread_id = Arc::clone(&hook_thread_id);
            if let Ok(worker) = thread::Builder::new()
                .name("snapbar-win-event-hook".to_string())
                .spawn(move || run_win_event_hook(signal, stop, hook_thread_id))
            {
                workers.push(worker);
            }
        }

        {
            let snapshot = Arc::clone(&snapshot);
            let signal = Arc::clone(&signal);
            let stop = Arc::clone(&stop);
            if let Ok(worker) = thread::Builder::new()
                .name("snapbar-meeting-monitor".to_string())
                .spawn(move || run_monitor(snapshot, signal, stop))
            {
                workers.push(worker);
            }
        }

        signal.request();
        Self {
            snapshot,
            signal,
            stop,
            hook_thread_id,
            workers,
        }
    }

    pub fn snapshot(&self) -> MeetingSnapshot {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default()
    }

    pub fn request_scan(&self) {
        self.signal.request();
    }
}

impl Drop for MeetingMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.signal.request();
        let thread_id = self.hook_thread_id.load(Ordering::Acquire);
        if thread_id != 0 {
            unsafe {
                let _ =
                    PostThreadMessageW(thread_id, WM_QUIT, Default::default(), Default::default());
            }
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
struct ScanSignal {
    pending: Mutex<bool>,
    wake: Condvar,
}

impl ScanSignal {
    fn request(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = true;
            self.wake.notify_all();
        }
    }

    fn wait(&self, stop: &AtomicBool) {
        let Ok(mut pending) = self.pending.lock() else {
            thread::sleep(WATCHDOG_INTERVAL);
            return;
        };
        if !*pending && !stop.load(Ordering::Acquire) {
            if let Ok((next, _)) = self.wake.wait_timeout(pending, WATCHDOG_INTERVAL) {
                pending = next;
            } else {
                return;
            }
        }
        *pending = false;
    }
}

#[derive(Clone, Debug)]
struct MeetingEvidence {
    target: CaptureTarget,
    minimized: bool,
    visible: bool,
    focused: bool,
    has_webview: bool,
    has_video: bool,
    has_leave_control: bool,
    has_shared_content: bool,
    score: i32,
}

impl MeetingEvidence {
    fn has_maintenance_signal(&self) -> bool {
        self.has_leave_control || self.has_video || self.has_shared_content
    }

    fn entry_delay(&self) -> Option<Duration> {
        if self.has_leave_control && (self.has_video || self.has_shared_content) {
            Some(ENTRY_STABLE_FOR)
        } else if self.has_leave_control || (self.has_video && self.has_shared_content) {
            Some(ENTRY_FALLBACK_STABLE_FOR)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug)]
struct EntryCandidate {
    target_id: u32,
    since: Instant,
    scans: u8,
}

#[derive(Default)]
struct DebouncedMeetingState {
    active: Option<MeetingEvidence>,
    entry: Option<EntryCandidate>,
    missing_since: Option<Instant>,
    missing_scans: u8,
}

impl DebouncedMeetingState {
    fn update(&mut self, now: Instant, evidence: Vec<MeetingEvidence>) -> Option<MeetingEvidence> {
        if let Some(active) = self.active.clone() {
            if let Some(current) = evidence
                .iter()
                .find(|candidate| candidate.target.id == active.target.id)
                .cloned()
            {
                if current.has_maintenance_signal() || current.minimized {
                    self.active = Some(current.clone());
                    self.missing_since = None;
                    self.missing_scans = 0;
                    self.entry = None;
                    return Some(current);
                }
            } else if raw_window_is_minimized(active.target.id) {
                let mut minimized = active;
                minimized.minimized = true;
                self.active = Some(minimized.clone());
                self.missing_since = None;
                self.missing_scans = 0;
                return Some(minimized);
            }

            self.missing_scans = self.missing_scans.saturating_add(1);
            let missing_since = *self.missing_since.get_or_insert(now);
            if self.missing_scans < REQUIRED_EXIT_SCANS
                || now.duration_since(missing_since) < EXIT_STABLE_FOR
            {
                return self.active.clone();
            }

            self.active = None;
            self.entry = None;
            self.missing_since = None;
            self.missing_scans = 0;
        }

        let best = evidence
            .into_iter()
            .filter(|candidate| candidate.entry_delay().is_some())
            .max_by_key(|candidate| candidate.score);
        let Some(best) = best else {
            self.entry = None;
            return None;
        };
        let required_delay = best.entry_delay().expect("filtered above");

        match self.entry.as_mut() {
            Some(entry) if entry.target_id == best.target.id => {
                entry.scans = entry.scans.saturating_add(1);
                if entry.scans >= REQUIRED_ENTRY_SCANS
                    && now.duration_since(entry.since) >= required_delay
                {
                    self.active = Some(best.clone());
                    self.entry = None;
                    return Some(best);
                }
            }
            _ => {
                self.entry = Some(EntryCandidate {
                    target_id: best.target.id,
                    since: now,
                    scans: 1,
                });
            }
        }

        None
    }
}

fn run_monitor(
    snapshot: Arc<Mutex<MeetingSnapshot>>,
    signal: Arc<ScanSignal>,
    stop: Arc<AtomicBool>,
) {
    let mut state = DebouncedMeetingState::default();
    while !stop.load(Ordering::Acquire) {
        signal.wait(&stop);
        if stop.load(Ordering::Acquire) {
            break;
        }

        let evidence = scan_meeting_windows().unwrap_or_default();
        let active = state.update(Instant::now(), evidence);
        if let Ok(mut current) = snapshot.lock() {
            let next_target = active.as_ref().map(|meeting| meeting.target.clone());
            let next_minimized = active.as_ref().is_some_and(|meeting| meeting.minimized);
            let next_shared = active
                .as_ref()
                .is_some_and(|meeting| meeting.has_shared_content);
            if current.target != next_target
                || current.minimized != next_minimized
                || current.shared_content_hint != next_shared
            {
                current.generation = current.generation.wrapping_add(1);
                current.target = next_target;
                current.minimized = next_minimized;
                current.shared_content_hint = next_shared;
            }
        }
    }
}

fn scan_meeting_windows() -> Result<Vec<MeetingEvidence>> {
    let windows = Window::all().context("Teamsウィンドウ一覧を取得できませんでした")?;
    let automation = UIAutomation::new()
        .or_else(|_| UIAutomation::new_direct())
        .ok();
    let mut meetings = Vec::new();

    for (z_index, window) in windows.into_iter().enumerate() {
        let id = match window.id() {
            Ok(id) => id,
            Err(_) => continue,
        };
        let app_name = window.app_name().unwrap_or_default();
        let title = window.title().unwrap_or_default();
        if !is_teams_window(&app_name, &title) {
            continue;
        }

        let hwnd = hwnd_from_id(id);
        if !unsafe { IsWindow(Some(hwnd)).as_bool() } {
            continue;
        }
        let minimized = unsafe { IsIconic(hwnd).as_bool() };
        let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
        let focused = window.is_focused().unwrap_or(false);
        let classes = inspect_child_classes(hwnd);
        let (has_leave_control, has_shared_content) = automation
            .as_ref()
            .map(|automation| inspect_meeting_uia(automation, id, &window))
            .transpose()
            .unwrap_or_default()
            .unwrap_or_default();

        let mut score = 0;
        if has_leave_control {
            score += 130;
        }
        if classes.has_video {
            score += 85;
        }
        if has_shared_content {
            score += 55;
        }
        if classes.has_webview {
            score += 20;
        }
        if focused {
            score += 18;
        }
        if visible {
            score += 8;
        }
        if !minimized {
            score += 5;
        }
        score += 20_i32.saturating_sub(z_index.min(20) as i32);

        meetings.push(MeetingEvidence {
            target: CaptureTarget {
                id,
                title,
                app_name,
            },
            minimized,
            visible,
            focused,
            has_webview: classes.has_webview,
            has_video: classes.has_video,
            has_leave_control,
            has_shared_content,
            score,
        });
    }

    meetings.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.focused.cmp(&left.focused))
            .then_with(|| right.visible.cmp(&left.visible))
            .then_with(|| right.has_webview.cmp(&left.has_webview))
    });
    Ok(meetings)
}

fn is_teams_window(app_name: &str, title: &str) -> bool {
    let app = app_name.to_lowercase();
    let title = title.to_lowercase();
    app.contains("ms-teams")
        || app.contains("teams")
        || title == "microsoft teams"
        || title.ends_with(" | microsoft teams")
        || title.ends_with(" - microsoft teams")
        || title.starts_with("microsoft teams |")
        || title.starts_with("microsoft teams -")
}

fn inspect_meeting_uia(
    automation: &UIAutomation,
    target_id: u32,
    window: &Window,
) -> Result<(bool, bool)> {
    let first = scan_meeting_uia(automation, target_id)?;
    if first.0 || first.1 {
        return Ok(first);
    }

    if let (Ok(x), Ok(y), Ok(width), Ok(height)) =
        (window.x(), window.y(), window.width(), window.height())
    {
        let point = Point::new(
            x.saturating_add(i32::try_from(width / 2).unwrap_or_default()),
            y.saturating_add(i32::try_from(height.saturating_mul(3) / 4).unwrap_or_default()),
        );
        let _ = automation.element_from_point(point);
        thread::sleep(PROVIDER_WARMUP_DELAY);
    }

    scan_meeting_uia(automation, target_id)
}

fn scan_meeting_uia(automation: &UIAutomation, target_id: u32) -> Result<(bool, bool)> {
    let root = automation
        .element_from_handle(Handle::from(target_id as isize))
        .context("Teams会議UIのルートを取得できませんでした")?;
    let condition = automation
        .create_true_condition()
        .context("Teams会議UIの検索条件を作成できませんでした")?;
    let elements = root
        .find_all(TreeScope::Subtree, &condition)
        .context("Teams会議UIを走査できませんでした")?;

    let mut has_leave = false;
    let mut has_shared_content = false;
    for element in elements {
        if element.is_offscreen().unwrap_or(true) {
            continue;
        }
        let name = normalize_name(&element.get_name().unwrap_or_default());
        let control_type = element.get_control_type().ok();
        if is_leave_control(&name, control_type) {
            has_leave = true;
        }
        if is_shared_content_name(&name) {
            has_shared_content = true;
        }
        if has_leave && has_shared_content {
            break;
        }
    }
    Ok((has_leave, has_shared_content))
}

fn is_leave_control(name: &str, control_type: Option<ControlType>) -> bool {
    if !matches!(
        control_type,
        Some(ControlType::Button | ControlType::SplitButton | ControlType::MenuItem)
    ) {
        return false;
    }

    [
        "退出",
        "通話を終了",
        "通話終了",
        "会議を終了",
        "leavecall",
        "leavemeeting",
        "leaveconference",
        "hangup",
        "endcall",
    ]
    .iter()
    .any(|hint| name.contains(hint))
}

fn is_shared_content_name(name: &str) -> bool {
    (name.contains("共有") && name.contains("コンテンツ"))
        || [
            "sharedcontent",
            "presentedcontent",
            "presentationcontent",
            "sharingcontent",
            "sharedscreen",
            "screensharing",
        ]
        .iter()
        .any(|hint| name.contains(hint))
}

fn normalize_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            !character.is_whitespace()
                && !matches!(character, '_' | '-' | '–' | '—' | '・' | '/' | '\\')
        })
        .flat_map(|character| character.to_lowercase())
        .collect()
}

#[derive(Default)]
struct ChildClassEvidence {
    has_webview: bool,
    has_video: bool,
}

fn inspect_child_classes(parent: HWND) -> ChildClassEvidence {
    let mut evidence = ChildClassEvidence::default();
    unsafe {
        let _ = EnumChildWindows(
            Some(parent),
            Some(enum_child_class),
            LPARAM((&mut evidence as *mut ChildClassEvidence).cast::<c_void>() as isize),
        );
    }
    evidence
}

unsafe extern "system" fn enum_child_class(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let evidence = unsafe { &mut *(lparam.0 as *mut ChildClassEvidence) };
    let mut buffer = [0_u16; 256];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if length > 0 {
        let class = String::from_utf16_lossy(&buffer[..length as usize]).to_lowercase();
        evidence.has_webview |= class.contains("teamswebview");
        evidence.has_video |= class.contains("teamsvideo");
    }
    BOOL(1)
}

fn raw_window_is_minimized(target_id: u32) -> bool {
    let hwnd = hwnd_from_id(target_id);
    unsafe { IsWindow(Some(hwnd)).as_bool() && IsIconic(hwnd).as_bool() }
}

fn hwnd_from_id(target_id: u32) -> HWND {
    HWND(target_id as usize as *mut c_void)
}

fn run_win_event_hook(signal: Arc<ScanSignal>, stop: Arc<AtomicBool>, thread_id: Arc<AtomicU32>) {
    thread_id.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
    let hooks = unsafe {
        [
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(win_event_callback),
                0,
                0,
                flags,
            ),
            SetWinEventHook(
                EVENT_SYSTEM_MINIMIZESTART,
                EVENT_SYSTEM_MINIMIZEEND,
                None,
                Some(win_event_callback),
                0,
                0,
                flags,
            ),
            SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_LOCATIONCHANGE,
                None,
                Some(win_event_callback),
                0,
                0,
                flags,
            ),
        ]
    };
    signal.request();

    let mut message = MSG::default();
    while !stop.load(Ordering::Acquire) {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
    }

    for hook in hooks {
        if !hook.is_invalid() {
            unsafe {
                let _ = UnhookWinEvent(hook);
            }
        }
    }
}

unsafe extern "system" fn win_event_callback(
    _: HWINEVENTHOOK,
    _: u32,
    _: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    if let Some(signal) = WIN_EVENT_SIGNAL.get().and_then(Weak::upgrade) {
        signal.request();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DebouncedMeetingState, MeetingEvidence, REQUIRED_ENTRY_SCANS, is_leave_control,
        is_shared_content_name, normalize_name,
    };
    use crate::capture::CaptureTarget;
    use std::time::{Duration, Instant};
    use uiautomation::types::ControlType;

    fn evidence(id: u32, leave: bool, video: bool, shared: bool) -> MeetingEvidence {
        MeetingEvidence {
            target: CaptureTarget {
                id,
                title: "Meeting | Microsoft Teams".to_string(),
                app_name: "ms-teams.exe".to_string(),
            },
            minimized: false,
            visible: true,
            focused: true,
            has_webview: true,
            has_video: video,
            has_leave_control: leave,
            has_shared_content: shared,
            score: 300,
        }
    }

    #[test]
    fn leave_control_names_are_strict() {
        assert!(is_leave_control(
            &normalize_name("会議から退出"),
            Some(ControlType::Button)
        ));
        assert!(is_leave_control(
            &normalize_name("Leave call"),
            Some(ControlType::MenuItem)
        ));
        assert!(!is_leave_control(
            &normalize_name("退出予定者"),
            Some(ControlType::Text)
        ));
    }

    #[test]
    fn shared_content_names_are_detected() {
        assert!(is_shared_content_name(&normalize_name("共有コンテンツ")));
        assert!(is_shared_content_name(&normalize_name("Shared content")));
        assert!(!is_shared_content_name(&normalize_name("共有を開始")));
    }

    #[test]
    fn meeting_entry_is_debounced() {
        let started = Instant::now();
        let mut state = DebouncedMeetingState::default();
        assert!(
            state
                .update(started, vec![evidence(42, true, true, false)])
                .is_none()
        );
        let active = state.update(
            started + Duration::from_millis(700),
            vec![evidence(42, true, true, false)],
        );
        assert_eq!(REQUIRED_ENTRY_SCANS, 2);
        assert_eq!(active.map(|meeting| meeting.target.id), Some(42));
    }

    #[test]
    fn transient_missing_signal_does_not_exit() {
        let started = Instant::now();
        let mut state = DebouncedMeetingState::default();
        let _ = state.update(started, vec![evidence(42, true, true, false)]);
        let _ = state.update(
            started + Duration::from_millis(700),
            vec![evidence(42, true, true, false)],
        );
        assert_eq!(
            state
                .update(started + Duration::from_millis(1_000), Vec::new())
                .map(|meeting| meeting.target.id),
            Some(42)
        );
    }
}
