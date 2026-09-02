use std::{
    ffi::c_void,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicIsize, Ordering},
    },
    thread::{self, JoinHandle},
};

use windows::{
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, HWND_MESSAGE,
                IDI_APPLICATION, LoadIconW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
                PostQuitMessage, RegisterClassW, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD,
                TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
                WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_RBUTTONUP,
                WNDCLASSW,
            },
        },
    },
    core::w,
};

use crate::shutdown::defer_cleanup;

const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 41;
const TRAY_ICON_ID: u32 = 1;
const MENU_RESCAN: u32 = 1001;
const MENU_QUIT: u32 = 1002;

static RESIDENT_FLAGS: OnceLock<Weak<ResidentFlags>> = OnceLock::new();

pub struct ResidentController {
    flags: Arc<ResidentFlags>,
    hwnd: Arc<AtomicIsize>,
    worker: Option<JoinHandle<()>>,
}

impl ResidentController {
    pub fn start() -> Self {
        let flags = Arc::new(ResidentFlags::default());
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
            worker,
        }
    }

    pub fn quit_requested(&self) -> bool {
        self.flags.quit.load(Ordering::Acquire)
    }

    pub fn take_rescan_requested(&self) -> bool {
        self.flags.rescan.swap(false, Ordering::AcqRel)
    }
}

impl Drop for ResidentController {
    fn drop(&mut self) {
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

#[derive(Default)]
struct ResidentFlags {
    quit: AtomicBool,
    rescan: AtomicBool,
    tray_error: Mutex<Option<String>>,
}

fn run_tray(flags: Arc<ResidentFlags>, hwnd_slot: Arc<AtomicIsize>) {
    if let Err(error) = unsafe { create_and_run_tray(&flags, &hwnd_slot) } {
        if let Ok(mut tray_error) = flags.tray_error.lock() {
            *tray_error = Some(error);
        }
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

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("SnapbarResidentWindow"),
            w!("Snapbar"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|error| error.to_string())?;
    hwnd_slot.store(hwnd.0 as isize, Ordering::Release);

    let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.map_err(|error| error.to_string())?;
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
            let _ = unsafe { DestroyWindow(hwnd) };
        }
    }

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
