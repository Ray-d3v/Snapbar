use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const LIMIT: u64 = 4 * 1024 * 1024;
thread_local! { static REQUEST: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
pub(crate) fn next_request() -> u64 {
    NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
}
pub(crate) struct RequestScope(u64);
pub(crate) fn request_scope(id: u64) -> RequestScope {
    RequestScope(REQUEST.replace(id))
}
impl Drop for RequestScope {
    fn drop(&mut self) {
        REQUEST.set(self.0);
    }
}
pub(crate) fn measure<T, E>(stage: &str, work: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let started = Instant::now();
    let result = work();
    log(format_args!(
        "stage={stage} duration_us={} success={}",
        started.elapsed().as_micros(),
        result.is_ok()
    ));
    result
}
static LOGGER: OnceLock<Logger> = OnceLock::new();
struct Logger {
    sender: SyncSender<Message>,
    dropped: Arc<AtomicU64>,
    started: Instant,
}
enum Message {
    Line(String),
    Open,
    Stop(SyncSender<()>),
}
pub(crate) struct Guard;

pub(crate) fn init() -> Guard {
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Snapbar")
        .join("Logs");
    let path = root.join(format!(
        "snapbar-{}-{}.log",
        timestamp(),
        std::process::id()
    ));
    let (sender, receiver) = mpsc::sync_channel(2048);
    let dropped = Arc::new(AtomicU64::new(0));
    let worker_dropped = dropped.clone();
    if let Err(error) = thread::Builder::new()
        .name("snapbar-audit-log".into())
        .spawn(move || {
            let run = || -> io::Result<()> {
                fs::create_dir_all(&root)?;
                prune(&root, 6)?;
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&path)?;
                let mut size = 0;
                while let Ok(message) = receiver.recv() {
                    match message {
                        Message::Line(line) => {
                            if size + line.len() as u64 > LIMIT {
                                file.flush()?;
                                drop(file);
                                let previous = path.with_extension("previous.log");
                                if previous.exists() {
                                    fs::remove_file(&previous)?;
                                }
                                fs::rename(&path, &previous)?;
                                file = File::create(&path)?;
                                size = 0;
                            }
                            let lost = worker_dropped.swap(0, Ordering::Relaxed);
                            if lost > 0 {
                                writeln!(file, "log_queue_dropped={lost}")?;
                            }
                            file.write_all(line.as_bytes())?;
                            size += line.len() as u64;
                        }
                        Message::Open => {
                            file.flush()?;
                            match export_log(&path).and_then(|snapshot| open_log_file(&snapshot)) {
                                Ok(()) => {
                                    writeln!(file, "open_log snapshot_launch_requested=true")?
                                }
                                Err(error) => {
                                    writeln!(file, "open_log_failed={error}")?;
                                    show_log_error();
                                }
                            }
                        }
                        Message::Stop(done) => {
                            file.flush()?;
                            let _ = done.send(());
                            break;
                        }
                    }
                }
                Ok(())
            };
            if let Err(error) = run() {
                eprintln!("Snapbar audit log unavailable: {error}");
            }
        })
    {
        eprintln!("Snapbar audit worker unavailable: {error}");
    }
    let _ = LOGGER.set(Logger {
        sender,
        dropped,
        started: Instant::now(),
    });
    log(format_args!(
        "session_start version={} pid={} arch={} os={} log_schema=1",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        std::env::consts::ARCH,
        std::env::consts::OS
    ));
    log(format_args!(
        "build={} logical_processors={:?}",
        env!("SNAPBAR_BUILD_ID"),
        thread::available_parallelism()
    ));
    Guard
}

fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
pub(crate) fn enabled() -> bool {
    LOGGER.get().is_some()
}
pub(crate) fn log(message: fmt::Arguments<'_>) {
    if let Some(logger) = LOGGER.get() {
        let line = format!(
            "utc_unix_ms={} elapsed_ms={} request={} thread={:?} {}\n",
            timestamp(),
            logger.started.elapsed().as_millis(),
            REQUEST.get(),
            thread::current().id(),
            message.to_string().replace(['\r', '\n'], " ")
        );
        if logger.sender.try_send(Message::Line(line)).is_err() {
            logger.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}
pub(crate) fn open_log() {
    if LOGGER
        .get()
        .is_none_or(|logger| logger.sender.try_send(Message::Open).is_err())
    {
        show_log_error();
    }
}
fn export_log(path: &Path) -> io::Result<PathBuf> {
    let root = std::env::temp_dir().join("SnapbarLogViewer");
    fs::create_dir_all(&root)?;
    prune(&root, 7)?;
    let destination = root.join(format!(
        "snapbar-{}-{}-{}.log",
        timestamp(),
        std::process::id(),
        next_request()
    ));
    let mut source = File::open(path)?;
    let mut copy = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)?;
    io::copy(&mut source, &mut copy)?;
    copy.flush()?;
    drop(copy);
    Ok(destination)
}

fn open_log_file(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize},
            UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        },
        core::{PCWSTR, w},
    };
    // Validate before invoking the viewer; do not open a new empty document.
    // Keep a normal Win32 path instead of canonicalize's verbatim-path prefix.
    let path = std::path::absolute(path)?;
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Log file is missing",
        ));
    }
    let mut argument = vec![u16::from(b'"')];
    argument.extend(path.as_os_str().encode_wide());
    argument.extend([u16::from(b'"'), 0]);
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    com.ok().map_err(io::Error::other)?;
    // Use Shell activation for Windows 11's packaged Notepad redirection.
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            w!("notepad.exe"),
            PCWSTR(argument.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    unsafe {
        CoUninitialize();
    }
    let code = result.0 as isize;
    if code <= 32 {
        return Err(io::Error::other(format!("ShellExecuteW failed: {code}")));
    }
    Ok(())
}

fn show_log_error() {
    use windows::{
        Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
        core::w,
    };
    unsafe {
        let _ = MessageBoxW(
            None,
            w!(
                "ログを開けませんでした。保存先の空き容量・アクセス権を確認してSnapbarを再起動してください。"
            ),
            w!("Snapbar"),
            MB_OK | MB_ICONERROR,
        );
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        log(format_args!("session_stop"));
        if let Some(logger) = LOGGER.get() {
            let (done, received) = mpsc::sync_channel(1);
            if logger.sender.try_send(Message::Stop(done)).is_ok() {
                let _ = received.recv_timeout(Duration::from_secs(1));
            }
        }
    }
}
fn prune(root: &Path, keep: usize) -> io::Result<()> {
    let mut files = fs::read_dir(root)?
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("snapbar-")
                && name.ends_with(".log")
                && entry.file_type().is_ok_and(|kind| kind.is_file())
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|entry| entry.file_name());
    let remove = files.len().saturating_sub(keep);
    for entry in files.into_iter().take(remove) {
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_log_is_rejected_before_launching_viewer() {
        let path = std::env::temp_dir().join(format!(
            "snapbar-missing-{}-{}.log",
            std::process::id(),
            timestamp()
        ));
        assert_eq!(
            open_log_file(&path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    #[ignore = "opens Notepad for interactive Windows verification"]
    fn open_actual_running_log() {
        let root = PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap()).join("Snapbar/Logs");
        let mut paths = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "log"))
            .collect::<Vec<_>>();
        paths.sort();
        let path = paths.last().unwrap();
        eprintln!("viewer path={path:?}");
        let snapshot = export_log(path).unwrap();
        open_log_file(&snapshot).unwrap();
    }

    #[test]
    #[ignore = "opens Notepad for interactive Windows verification"]
    fn open_unicode_log_in_notepad() {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/診断ログ opening test.log");
        fs::write(
            &path,
            "Snapbar log viewer verification\n日本語・空白を含むパスの読み取り確認\n",
        )
        .unwrap();
        open_log_file(&path).unwrap();
    }
    #[test]
    fn request_scope_restores_context_and_does_not_cross_threads() {
        let original = REQUEST.get();
        {
            let _scope = request_scope(42);
            assert_eq!(REQUEST.get(), 42);
            assert_eq!(thread::spawn(|| REQUEST.get()).join().unwrap(), 0);
            {
                let _nested = request_scope(43);
                assert_eq!(REQUEST.get(), 43);
            }
            assert_eq!(REQUEST.get(), 42);
        }
        assert_eq!(REQUEST.get(), original);
    }
    #[test]
    fn measurement_preserves_success_and_failure() {
        assert_eq!(measure("test", || Ok::<_, &str>(17)), Ok(17));
        assert_eq!(measure("test", || Err::<(), _>("failed")), Err("failed"));
    }
    #[test]
    fn retention_only_removes_old_log_files() {
        let root = std::env::temp_dir().join(format!(
            "snapbar-log-test-{}-{}",
            std::process::id(),
            timestamp()
        ));
        fs::create_dir(&root).unwrap();
        for name in [
            "snapbar-100.log",
            "snapbar-200.log",
            "snapbar-300.log",
            "notes.txt",
        ] {
            fs::write(root.join(name), "test").unwrap();
        }
        prune(&root, 2).unwrap();
        assert!(!root.join("snapbar-100.log").exists());
        for name in ["snapbar-200.log", "snapbar-300.log", "notes.txt"] {
            assert!(root.join(name).exists());
            fs::remove_file(root.join(name)).unwrap();
        }
        fs::remove_dir(root).unwrap();
    }
}
