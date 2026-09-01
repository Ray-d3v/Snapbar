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
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM},
        System::Threading::GetCurrentThreadId,
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            WindowsAndMessaging::{
                EVENT_OBJECT_CREATE, EVENT_OBJECT_LOCATIONCHANGE, EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EnumChildWindows,
                GWL_EXSTYLE, GetClassNameW, GetMessageW, GetWindowDisplayAffinity, GetWindowLongW,
                IsIconic, IsWindow, IsWindowVisible, MSG, PostThreadMessageW,
                WDA_EXCLUDEFROMCAPTURE, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_QUIT,
                WS_EX_TOPMOST,
            },
        },
    },
    core::BOOL,
};
use xcap::Window;

use crate::capture::{CaptureTarget, LocalMonitorCaptureTarget, detect_local_monitor_target};

const WATCHDOG_INTERVAL: Duration = Duration::from_millis(700);
const PROVIDER_WARMUP_DELAY: Duration = Duration::from_millis(40);
const ENTRY_STABLE_FOR: Duration = Duration::from_millis(600);
const ENTRY_FALLBACK_STABLE_FOR: Duration = Duration::from_millis(1_100);
const EXIT_STABLE_FOR: Duration = Duration::from_millis(1_400);
const PRESENTER_TOOLBAR_EXIT_STABLE_FOR: Duration = Duration::from_millis(450);
const REQUIRED_ENTRY_SCANS: u8 = 2;
const REQUIRED_EXIT_SCANS: u8 = 2;
const REQUIRED_PRESENTER_TOOLBAR_EXIT_SCANS: u8 = 2;

static WIN_EVENT_SIGNAL: OnceLock<Weak<ScanSignal>> = OnceLock::new();

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeetingSnapshot {
    pub generation: u64,
    pub target: Option<CaptureTarget>,
    pub minimized: bool,
    pub shared_content_hint: bool,
    pub presenter_toolbar_id: Option<u32>,
    pub local_share_active: bool,
    pub local_monitor_target: Option<LocalMonitorCaptureTarget>,
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
    has_call_control: bool,
    has_shared_content: bool,
    score: i32,
}

#[derive(Default)]
struct TeamsScan {
    meetings: Vec<MeetingEvidence>,
    presenter_toolbars: Vec<PresenterToolbarEvidence>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PresenterToolbarEvidence {
    id: u32,
    score: i32,
    local_share_active: bool,
}

#[derive(Default)]
struct PresenterToolbarState {
    active: Option<u32>,
    missing_since: Option<Instant>,
    missing_scans: u8,
}

impl PresenterToolbarState {
    fn update(&mut self, now: Instant, evidence: Vec<PresenterToolbarEvidence>) -> Option<u32> {
        if let Some(active) = self.active {
            if evidence.iter().any(|candidate| candidate.id == active) {
                self.missing_since = None;
                self.missing_scans = 0;
                return Some(active);
            }

            if let Some(replacement) = evidence.iter().max_by_key(|candidate| candidate.score) {
                self.active = Some(replacement.id);
                self.missing_since = None;
                self.missing_scans = 0;
                return self.active;
            }

            self.missing_scans = self.missing_scans.saturating_add(1);
            let missing_since = *self.missing_since.get_or_insert(now);
            if self.missing_scans < REQUIRED_PRESENTER_TOOLBAR_EXIT_SCANS
                || now.duration_since(missing_since) < PRESENTER_TOOLBAR_EXIT_STABLE_FOR
            {
                return Some(active);
            }

            self.active = None;
            self.missing_since = None;
            self.missing_scans = 0;
        }

        self.active = evidence
            .into_iter()
            .max_by_key(|candidate| candidate.score)
            .map(|candidate| candidate.id);
        self.active
    }
}

impl MeetingEvidence {
    fn has_maintenance_signal(&self) -> bool {
        self.has_leave_control || self.has_call_control || self.has_video || self.has_shared_content
    }

    fn entry_delay(&self) -> Option<Duration> {
        if (self.has_leave_control
            && (self.has_call_control || self.has_video || self.has_shared_content))
            || (self.has_call_control && (self.has_video || self.has_shared_content))
        {
            Some(ENTRY_STABLE_FOR)
        } else if self.has_leave_control
            || self.has_call_control
            || (self.has_video && self.has_shared_content)
        {
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
    let mut presenter_toolbar = PresenterToolbarState::default();
    while !stop.load(Ordering::Acquire) {
        signal.wait(&stop);
        if stop.load(Ordering::Acquire) {
            break;
        }

        let now = Instant::now();
        let scan = scan_meeting_windows().unwrap_or_default();
        let active = state.update(now, scan.meetings);
        let presenter_evidence = scan.presenter_toolbars;
        let next_presenter_toolbar = presenter_toolbar.update(now, presenter_evidence.clone());
        let next_local_share_active = next_presenter_toolbar.is_some_and(|active_id| {
            presenter_evidence
                .iter()
                .find(|candidate| candidate.id == active_id)
                .is_some_and(|candidate| candidate.local_share_active)
        });
        let next_local_monitor_target = next_local_share_active
            .then(detect_local_monitor_target)
            .transpose()
            .unwrap_or_default()
            .flatten();
        if let Ok(mut current) = snapshot.lock() {
            let next_target = active.as_ref().map(|meeting| meeting.target.clone());
            let next_minimized = active.as_ref().is_some_and(|meeting| meeting.minimized);
            let next_shared = active
                .as_ref()
                .is_some_and(|meeting| meeting.has_shared_content);
            if current.target != next_target
                || current.minimized != next_minimized
                || current.shared_content_hint != next_shared
                || current.presenter_toolbar_id != next_presenter_toolbar
                || current.local_share_active != next_local_share_active
                || current.local_monitor_target != next_local_monitor_target
            {
                current.generation = current.generation.wrapping_add(1);
                current.target = next_target;
                current.minimized = next_minimized;
                current.shared_content_hint = next_shared;
                current.presenter_toolbar_id = next_presenter_toolbar;
                current.local_share_active = next_local_share_active;
                current.local_monitor_target = next_local_monitor_target;
            }
        }
    }
}

fn scan_meeting_windows() -> Result<TeamsScan> {
    let windows = Window::all().context("Teamsウィンドウ一覧を取得できませんでした")?;
    let automation = UIAutomation::new()
        .or_else(|_| UIAutomation::new_direct())
        .ok();
    let mut scan = TeamsScan::default();

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
        let width = i32::try_from(window.width().unwrap_or_default()).unwrap_or(i32::MAX);
        let height = i32::try_from(window.height().unwrap_or_default()).unwrap_or(i32::MAX);
        let topmost = window_is_topmost(hwnd);
        let display_affinity = window_display_affinity(hwnd);
        if let Some(mut score) =
            presenter_toolbar_score(&title, width, height, visible, topmost, display_affinity)
        {
            let local_share_active = automation.as_ref().is_some_and(|automation| {
                inspect_presenter_toolbar_uia(automation, id).unwrap_or(false)
            });
            if local_share_active {
                score += 40;
            }
            scan.presenter_toolbars.push(PresenterToolbarEvidence {
                id,
                score,
                local_share_active,
            });
            continue;
        }

        let classes = inspect_child_classes(hwnd);
        let uia = automation
            .as_ref()
            .map(|automation| inspect_meeting_uia(automation, id, &window))
            .transpose()
            .unwrap_or_default()
            .unwrap_or_default();

        let mut score = 0;
        if uia.has_leave_control {
            score += 130;
        }
        if uia.has_call_control {
            score += 110;
        }
        if classes.has_video {
            score += 85;
        }
        if uia.has_shared_content {
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

        scan.meetings.push(MeetingEvidence {
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
            has_leave_control: uia.has_leave_control,
            has_call_control: uia.has_call_control,
            has_shared_content: uia.has_shared_content,
            score,
        });
    }

    scan.meetings.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.focused.cmp(&left.focused))
            .then_with(|| right.visible.cmp(&left.visible))
            .then_with(|| right.has_webview.cmp(&left.has_webview))
    });
    scan.presenter_toolbars
        .sort_by_key(|candidate| std::cmp::Reverse(candidate.score));
    Ok(scan)
}

fn presenter_toolbar_score(
    title: &str,
    width: i32,
    height: i32,
    visible: bool,
    topmost: bool,
    display_affinity: Option<u32>,
) -> Option<i32> {
    if !(220..=1_600).contains(&width)
        || !(24..=100).contains(&height)
        || width < height.saturating_mul(3)
    {
        return None;
    }

    let title = normalize_name(title);
    let named_toolbar = [
        "共有コントロールバー",
        "共有ツールバー",
        "発表者ツールバー",
        "sharecontrolbar",
        "sharingcontrolbar",
        "sharingtoolbar",
        "presentertoolbar",
        "presentationtoolbar",
    ]
    .iter()
    .any(|hint| title.contains(hint));
    let excluded_from_capture = display_affinity == Some(WDA_EXCLUDEFROMCAPTURE.0);

    // The toolbar title changes to the meeting title after it is expanded or
    // activated. Its compact horizontal geometry, capture exclusion, and
    // topmost band remain stable enough to retain the same HWND.
    if !named_toolbar && !(excluded_from_capture && topmost) {
        return None;
    }

    let mut score = 0;
    if named_toolbar {
        score += 100;
    }
    if excluded_from_capture {
        score += 60;
    }
    if topmost {
        score += 20;
    }
    if visible {
        score += 10;
    }
    Some(score)
}

fn window_display_affinity(hwnd: HWND) -> Option<u32> {
    let mut affinity = 0_u32;
    unsafe {
        GetWindowDisplayAffinity(hwnd, &mut affinity).ok()?;
    }
    Some(affinity)
}

fn window_is_topmost(hwnd: HWND) -> bool {
    unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0 }
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

fn inspect_presenter_toolbar_uia(automation: &UIAutomation, target_id: u32) -> Result<bool> {
    let root = automation
        .element_from_handle(Handle::from(target_id as isize))
        .context("Teams発表者ツールバーのUIAルートを取得できませんでした")?;
    let condition = automation
        .create_true_condition()
        .context("Teams発表者ツールバーのUIA検索条件を作成できませんでした")?;
    let elements = root
        .find_all(TreeScope::Subtree, &condition)
        .context("Teams発表者ツールバーのUIAを走査できませんでした")?;

    Ok(elements.into_iter().any(|element| {
        if element.is_offscreen().unwrap_or(true) {
            return false;
        }
        let name = normalize_name(&element.get_name().unwrap_or_default());
        let automation_id = element.get_automation_id().unwrap_or_default();
        presenter_element_confirms_local_share(
            &name,
            &automation_id,
            element.get_control_type().ok(),
        )
    }))
}

fn presenter_element_confirms_local_share(
    name: &str,
    automation_id: &str,
    control_type: Option<ControlType>,
) -> bool {
    let is_button = matches!(
        control_type,
        Some(ControlType::Button | ControlType::SplitButton | ControlType::MenuItem)
    );
    let stop_action = (name.contains("共有") && (name.contains("停止") || name.contains("終了")))
        || ["stopsharing", "stopshare", "endsharing", "stoppresenting"]
            .iter()
            .any(|hint| name.contains(hint));
    let sharing_status = (name.contains("共有しています")
        && (name.contains("画面") || name.contains("スクリーン") || name.contains("ウィンドウ")))
        || [
            "sharingyourscreen",
            "sharingyourwindow",
            "youaresharing",
            "youresharing",
        ]
        .iter()
        .any(|hint| name.contains(hint));

    (is_button && automation_id.eq_ignore_ascii_case("share-button") && stop_action)
        || (is_button && sharing_status)
}

fn inspect_meeting_uia(
    automation: &UIAutomation,
    target_id: u32,
    window: &Window,
) -> Result<MeetingUiaEvidence> {
    let first = scan_meeting_uia(automation, target_id)?;
    if first.has_signal() {
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

#[derive(Clone, Copy, Debug, Default)]
struct MeetingUiaEvidence {
    has_leave_control: bool,
    has_call_control: bool,
    has_shared_content: bool,
}

impl MeetingUiaEvidence {
    fn has_signal(self) -> bool {
        self.has_leave_control || self.has_call_control || self.has_shared_content
    }
}

fn scan_meeting_uia(automation: &UIAutomation, target_id: u32) -> Result<MeetingUiaEvidence> {
    let root = automation
        .element_from_handle(Handle::from(target_id as isize))
        .context("Teams会議UIのルートを取得できませんでした")?;
    let condition = automation
        .create_true_condition()
        .context("Teams会議UIの検索条件を作成できませんでした")?;
    let elements = root
        .find_all(TreeScope::Subtree, &condition)
        .context("Teams会議UIを走査できませんでした")?;

    let mut evidence = MeetingUiaEvidence::default();
    for element in elements {
        if element.is_offscreen().unwrap_or(true) {
            continue;
        }
        let name = normalize_name(&element.get_name().unwrap_or_default());
        let automation_id = element.get_automation_id().unwrap_or_default();
        let control_type = element.get_control_type().ok();
        if is_leave_control(&name, control_type) {
            evidence.has_leave_control = true;
        }
        if is_call_control(&automation_id, control_type) {
            evidence.has_call_control = true;
        }
        if is_shared_content_name(&name) {
            evidence.has_shared_content = true;
        }
        if evidence.has_leave_control && evidence.has_call_control && evidence.has_shared_content {
            break;
        }
    }
    Ok(evidence)
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

fn is_call_control(automation_id: &str, control_type: Option<ControlType>) -> bool {
    let automation_id = automation_id.trim();
    (automation_id.eq_ignore_ascii_case("hangup-button")
        && matches!(
            control_type,
            Some(ControlType::Button | ControlType::SplitButton | ControlType::MenuItem)
        ))
        || (automation_id.eq_ignore_ascii_case("call-duration-custom")
            && matches!(control_type, Some(ControlType::Text)))
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
                && !matches!(
                    character,
                    '_' | '-' | '–' | '—' | '・' | '/' | '\\' | '\'' | '’'
                )
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
        DebouncedMeetingState, MeetingEvidence, PresenterToolbarEvidence, PresenterToolbarState,
        REQUIRED_ENTRY_SCANS, is_call_control, is_leave_control, is_shared_content_name,
        normalize_name, presenter_element_confirms_local_share, presenter_toolbar_score,
    };
    use crate::capture::CaptureTarget;
    use std::time::{Duration, Instant};
    use uiautomation::types::ControlType;
    use windows::Win32::UI::WindowsAndMessaging::WDA_EXCLUDEFROMCAPTURE;

    fn evidence(
        id: u32,
        leave: bool,
        call_control: bool,
        video: bool,
        shared: bool,
    ) -> MeetingEvidence {
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
            has_call_control: call_control,
            has_shared_content: shared,
            score: 300,
        }
    }

    fn toolbar_evidence(id: u32) -> PresenterToolbarEvidence {
        PresenterToolbarEvidence {
            id,
            score: 190,
            local_share_active: true,
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
    fn stable_call_control_ids_are_detected() {
        assert!(is_call_control("hangup-button", Some(ControlType::Button)));
        assert!(is_call_control(
            "call-duration-custom",
            Some(ControlType::Text)
        ));
        assert!(!is_call_control("share-button", Some(ControlType::Button)));
    }

    #[test]
    fn presenter_toolbar_requires_an_active_local_share_action() {
        assert!(presenter_element_confirms_local_share(
            &normalize_name("共有を停止"),
            "share-button",
            Some(ControlType::Button),
        ));
        assert!(presenter_element_confirms_local_share(
            &normalize_name("You're sharing your screen"),
            "",
            Some(ControlType::Button),
        ));
        assert!(!presenter_element_confirms_local_share(
            &normalize_name("共有"),
            "share-button",
            Some(ControlType::Button),
        ));
    }

    #[test]
    fn presenter_toolbar_is_detected_before_and_after_its_title_changes() {
        assert!(
            presenter_toolbar_score(
                "共有コントロール バー | Microsoft Teams",
                297,
                35,
                true,
                true,
                None,
            )
            .is_some()
        );
        assert!(
            presenter_toolbar_score(
                "Meeting | Microsoft Teams",
                790,
                55,
                true,
                true,
                Some(WDA_EXCLUDEFROMCAPTURE.0),
            )
            .is_some()
        );
    }

    #[test]
    fn ordinary_teams_windows_are_not_presenter_toolbars() {
        assert!(
            presenter_toolbar_score("Meeting | Microsoft Teams", 1_200, 800, true, false, None,)
                .is_none()
        );
        assert!(
            presenter_toolbar_score("Calendar | Microsoft Teams", 900, 55, true, true, None,)
                .is_none()
        );
    }

    #[test]
    fn presenter_toolbar_survives_one_missing_scan() {
        let started = Instant::now();
        let mut state = PresenterToolbarState::default();
        let evidence = toolbar_evidence(77);
        assert_eq!(state.update(started, vec![evidence]), Some(77));
        assert_eq!(
            state.update(started + Duration::from_millis(200), Vec::new()),
            Some(77)
        );
        assert_eq!(
            state.update(started + Duration::from_millis(700), Vec::new()),
            None
        );
    }

    #[test]
    fn presenter_toolbar_switches_immediately_when_teams_recreates_its_hwnd() {
        let started = Instant::now();
        let mut state = PresenterToolbarState::default();
        assert_eq!(state.update(started, vec![toolbar_evidence(77)]), Some(77));
        assert_eq!(
            state.update(
                started + Duration::from_millis(10),
                vec![toolbar_evidence(88)]
            ),
            Some(88)
        );
    }

    #[test]
    fn meeting_entry_is_debounced() {
        let started = Instant::now();
        let mut state = DebouncedMeetingState::default();
        assert!(
            state
                .update(started, vec![evidence(42, true, true, true, false)])
                .is_none()
        );
        let active = state.update(
            started + Duration::from_millis(700),
            vec![evidence(42, true, true, true, false)],
        );
        assert_eq!(REQUIRED_ENTRY_SCANS, 2);
        assert_eq!(active.map(|meeting| meeting.target.id), Some(42));
    }

    #[test]
    fn single_participant_meeting_enters_without_shared_content() {
        let started = Instant::now();
        let mut state = DebouncedMeetingState::default();
        assert!(
            state
                .update(started, vec![evidence(42, false, true, false, false)])
                .is_none()
        );
        let active = state.update(
            started + Duration::from_millis(1_200),
            vec![evidence(42, false, true, false, false)],
        );
        assert_eq!(active.map(|meeting| meeting.target.id), Some(42));
    }

    #[test]
    fn shared_content_alone_does_not_enter_a_meeting() {
        let started = Instant::now();
        let mut state = DebouncedMeetingState::default();
        assert!(
            state
                .update(started, vec![evidence(42, false, false, false, true)])
                .is_none()
        );
        assert!(
            state
                .update(
                    started + Duration::from_millis(2_000),
                    vec![evidence(42, false, false, false, true)],
                )
                .is_none()
        );
    }

    #[test]
    fn transient_missing_signal_does_not_exit() {
        let started = Instant::now();
        let mut state = DebouncedMeetingState::default();
        let _ = state.update(started, vec![evidence(42, true, true, true, false)]);
        let _ = state.update(
            started + Duration::from_millis(700),
            vec![evidence(42, true, true, true, false)],
        );
        assert_eq!(
            state
                .update(started + Duration::from_millis(1_000), Vec::new())
                .map(|meeting| meeting.target.id),
            Some(42)
        );
    }
}
