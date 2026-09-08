use std::{fmt, sync::OnceLock, time::Instant};

pub(crate) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::args().any(|arg| arg == "--diagnostics"))
}

pub(crate) fn log(message: fmt::Arguments<'_>) {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    if enabled() {
        eprintln!(
            "[{}ms] {message}",
            STARTED.get_or_init(Instant::now).elapsed().as_millis()
        );
    }
}
