use std::{
    ffi::c_void,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
    thread::{self, JoinHandle},
};

use async_channel::{Receiver, Sender};
use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{
                MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey, VK_S,
            },
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
                IMAGE_ICON, LR_SHARED, LoadImageW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
                PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON,
                SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
                TranslateMessage, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_HOTKEY,
                WM_LBUTTONDBLCLK, WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
            },
        },
    },
    core::{PCWSTR, w},
};

use crate::shutdown::defer_cleanup;

const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 41;
const SYNC_CAPTURE_HOTKEY_MESSAGE: u32 = WM_APP + 42;
const TRAY_ICON_ID: u32 = 1;
const MENU_RESCAN: u32 = 1001;
const MENU_QUIT: u32 = 1002;
const CAPTURE_HOTKEY_ID: i32 = 1;
const APP_ICON_RESOURCE_ID: u16 = 101;

static RESIDENT_FLAGS: OnceLock<Weak<ResidentFlags>> = OnceLock::new();
static TASKBAR_CREATED_MESSAGE: OnceLock<u32> = OnceLock::new();

pub struct ResidentController {
    flags: Arc<ResidentFlags>,
    hwnd: Arc<AtomicIsize>,
    capture_requests: Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl ResidentController {
    pub fn start() -> Self {
        let (capture_sender, capture_requests) = async_channel::bounded(1);
        let flags = Arc::new(ResidentFlags::new(capture_sender));
        let hwnd = Arc::new(AtomicIsize::new(0));
        let _ = RESIDENT_FLAGS.set(Arc::downgrade(&flags));

        let thread_flags = Arc::clone(&flags);
        let thread_hwnd = Arc::clone(&hwnd);
        let worker = thread::Builder::new()
            .name("snapbar-notification-area".to_string())
            .spawn(move || run_tray(thread_flags, thread_hwnd))
            .ok();

        Self {
            flags,
            hwnd,
            capture_requests,
            worker,
        }
    }

    pub fn quit_requested(&self) -> bool {
        self.flags.quit.load(Ordering::Acquire)
    }

    pub fn take_rescan_requested(&self) -> bool {
        self.flags.rescan.swap(false, Ordering::AcqRel)
    }

    pub fn capture_requests(&self) -> Receiver<()> {
        self.capture_requests.clone()
    }

    pub fn set_capture_hotkey_enabled(&self, enabled: bool) {
        if self.flags.hotkey_enabled.swap(enabled, Ordering::AcqRel) == enabled {
            return;
        }

        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut c_void)),
                    SYNC_CAPTURE_HOTKEY_MESSAGE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

impl Drop for ResidentController {
    fn drop(&mut self) {
        self.flags.hotkey_enabled.store(false, Ordering::Release);
        self.flags.quit.store(true, Ordering::Release);
        let hwnd = self.hwnd.load(Ordering::Acquire);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut c_void)),
                    WM_CLOSE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
        if let Some(worker) = self.worker.take() {
            defer_cleanup("snapbar-notification-area-stop", move || {
                let _ = worker.join();
            });
        }
    }
}

struct ResidentFlags {
    quit: AtomicBool,
    rescan: AtomicBool,
    hotkey_enabled: AtomicBool,
    hotkey_registered: AtomicBool,
    capture_sender: Sender<()>,
    tray_error: Mutex<Option<String>>,
}

impl ResidentFlags {
    fn new(capture_sender: Sender<()>) -> Self {
        Self {
            quit: AtomicBool::new(false),
            rescan: AtomicBool::new(false),
            hotkey_enabled: AtomicBool::new(false),
            hotkey_registered: AtomicBool::new(false),
            capture_sender,
            tray_error: Mutex::new(None),
        }
    }

    fn request_capture(&self) {
        let _ = self.capture_sender.try_send(());
    }

    fn record_error(&self, error: String) {
        if let Ok(mut tray_error) = self.tray_error.lock() {
            *tray_error = Some(error);
        }
    }
}

fn run_tray(flags: Arc<ResidentFlags>, hwnd_slot: Arc<AtomicIsize>) {
    if let Err(error) = unsafe { create_and_run_tray(&flags, &hwnd_slot) } {
        flags.record_error(error);
    }
}

unsafe fn create_and_run_tray(
    flags: &ResidentFlags,
    hwnd_slot: &AtomicIsize,
) -> Result<(), String> {
    let module = unsafe { GetModuleHandleW(None) }.map_err(|error| error.to_string())?;
    let instance = HINSTANCE(module.0);
    let class = WNDCLASSW {
        lpfnWndProc: Some(tray_window_proc),
        hInstance: instance,
        lpszClassName: w!("SnapbarResidentWindow"),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err("通知領域ウィンドウを登録できませんでした".to_string());
    }
    let taskbar_created_message = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if taskbar_created_message == 0 {
        return Err("タスクバー再作成通知を登録できませんでした".to_string());
    }
    let _ = TASKBAR_CREATED_MESSAGE.set(taskbar_created_message);

    let icon = unsafe {
        LoadImageW(
            Some(instance),
            PCWSTR(APP_ICON_RESOURCE_ID as usize as *const u16),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        )
    }
    .map_err(|error| error.to_string())?;
    let icon = windows::Win32::UI::WindowsAndMessaging::HICON(icon.0);

    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            w!("SnapbarResidentWindow"),
            w!("Snapbar"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|error| error.to_string())?;
    hwnd_slot.store(hwnd.0 as isize, Ordering::Release);

    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: TRAY_CALLBACK_MESSAGE,
        hIcon: icon,
        ..Default::default()
    };
    copy_utf16("Snapbar – Teams会議を監視中", &mut data.szTip);
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        let _ = unsafe { DestroyWindow(hwnd) };
        return Err("通知領域へSnapbarを追加できませんでした".to_string());
    }
    unsafe { sync_capture_hotkey(hwnd, flags) };

    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if flags.quit.load(Ordering::Acquire) {
            let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        }
    }

    unsafe { unregister_capture_hotkey(hwnd, flags) };
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
    hwnd_slot.store(0, Ordering::Release);
    Ok(())
}

unsafe extern "system" fn tray_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        message if TASKBAR_CREATED_MESSAGE.get().copied() == Some(message) => {
            with_flags(|flags| unsafe {
                if !restore_tray_icon(hwnd, flags) {
                    flags.record_error("通知領域アイコンを復元できませんでした".to_string());
                }
            });
            LRESULT(0)
        }
        WM_HOTKEY if wparam.0 == CAPTURE_HOTKEY_ID as usize => {
            with_flags(ResidentFlags::request_capture);
            LRESULT(0)
        }
        SYNC_CAPTURE_HOTKEY_MESSAGE => {
            with_flags(|flags| unsafe { sync_capture_hotkey(hwnd, flags) });
            LRESULT(0)
        }
        TRAY_CALLBACK_MESSAGE => {
            let mouse_message = lparam.0 as u32;
            if mouse_message == WM_RBUTTONUP || mouse_message == WM_CONTEXTMENU {
                unsafe { show_tray_menu(hwnd) };
            } else if mouse_message == WM_LBUTTONDBLCLK {
                with_flags(|flags| flags.rescan.store(true, Ordering::Release));
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            with_flags(|flags| unsafe { unregister_capture_hotkey(hwnd, flags) });
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe fn restore_tray_icon(hwnd: HWND, flags: &ResidentFlags) -> bool {
    let Ok(module) = (unsafe { GetModuleHandleW(None) }) else {
        return false;
    };
    let instance = HINSTANCE(module.0);
    let Ok(icon) = (unsafe {
        LoadImageW(
            Some(instance),
            PCWSTR(APP_ICON_RESOURCE_ID as usize as *const u16),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        )
    }) else {
        return false;
    };
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: TRAY_CALLBACK_MESSAGE,
        hIcon: windows::Win32::UI::WindowsAndMessaging::HICON(icon.0),
        ..Default::default()
    };
    let tooltip = if flags.hotkey_enabled.load(Ordering::Acquire) {
        "Snapbar – Ctrl + Alt + S で撮影"
    } else {
        "Snapbar – Teams会議を監視中"
    };
    copy_utf16(tooltip, &mut data.szTip);
    unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool()
}

unsafe fn sync_capture_hotkey(hwnd: HWND, flags: &ResidentFlags) {
    let enabled = flags.hotkey_enabled.load(Ordering::Acquire);
    let registered = flags.hotkey_registered.load(Ordering::Acquire);
    match (enabled, registered) {
        (true, false) => {
            let modifiers = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
            match unsafe { RegisterHotKey(Some(hwnd), CAPTURE_HOTKEY_ID, modifiers, VK_S.0 as u32) }
            {
                Ok(()) => {
                    flags.hotkey_registered.store(true, Ordering::Release);
                    unsafe { update_tray_tooltip(hwnd, "Snapbar – Ctrl + Alt + S で撮影") };
                }
                Err(error) => {
                    flags.record_error(format!(
                        "Ctrl + Alt + S をグローバルショートカットとして登録できませんでした: {error}"
                    ));
                    unsafe {
                        update_tray_tooltip(hwnd, "Snapbar – ショートカット登録失敗")
                    };
                }
            }
        }
        (false, true) => unsafe { unregister_capture_hotkey(hwnd, flags) },
        _ => {}
    }
}

unsafe fn unregister_capture_hotkey(hwnd: HWND, flags: &ResidentFlags) {
    if !flags.hotkey_registered.load(Ordering::Acquire) {
        return;
    }

    match unsafe { UnregisterHotKey(Some(hwnd), CAPTURE_HOTKEY_ID) } {
        Ok(()) => {
            flags.hotkey_registered.store(false, Ordering::Release);
            unsafe { update_tray_tooltip(hwnd, "Snapbar – Teams会議を監視中") };
        }
        Err(error) => flags.record_error(format!(
            "Ctrl + Alt + S のグローバルショートカットを解除できませんでした: {error}"
        )),
    }
}

unsafe fn update_tray_tooltip(hwnd: HWND, value: &str) {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_TIP,
        ..Default::default()
    };
    copy_utf16(value, &mut data.szTip);
    let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    let _ = unsafe { AppendMenuW(menu, MF_STRING, MENU_RESCAN as usize, w!("会議を再検出")) };
    let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, w!("")) };
    let _ = unsafe { AppendMenuW(menu, MF_STRING, MENU_QUIT as usize, w!("Snapbarを終了")) };

    let mut cursor = POINT::default();
    if unsafe { GetCursorPos(&mut cursor) }.is_ok() {
        let _ = unsafe { SetForegroundWindow(hwnd) };
        let command = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
                cursor.x,
                cursor.y,
                None,
                hwnd,
                None,
            )
        }
        .0 as u32;
        match command {
            MENU_RESCAN => with_flags(|flags| flags.rescan.store(true, Ordering::Release)),
            MENU_QUIT => with_flags(|flags| flags.quit.store(true, Ordering::Release)),
            _ => {}
        }
    }
    let _ = unsafe { DestroyMenu(menu) };
}

fn with_flags(action: impl FnOnce(&ResidentFlags)) {
    if let Some(flags) = RESIDENT_FLAGS.get().and_then(Weak::upgrade) {
        action(&flags);
    }
}

fn copy_utf16(value: &str, destination: &mut [u16]) {
    if destination.is_empty() {
        return;
    }
    let encoded: Vec<u16> = value.encode_utf16().collect();
    let length = encoded.len().min(destination.len() - 1);
    destination[..length].copy_from_slice(&encoded[..length]);
    destination[length] = 0;
}

#[cfg(test)]
mod tests {
    use super::ResidentFlags;

    #[test]
    fn capture_requests_are_coalesced_until_the_ui_receives_one() {
        let (sender, receiver) = async_channel::bounded(1);
        let flags = ResidentFlags::new(sender);

        flags.request_capture();
        flags.request_capture();

        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
    }
}
