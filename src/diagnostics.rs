use std::{fmt, sync::OnceLock, time::Instant};

pub(crate) fn log(message: fmt::Arguments<'_>) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    static STARTED: OnceLock<Instant> = OnceLock::new();
    if *ENABLED.get_or_init(|| std::env::args().any(|arg| arg == "--diagnostics")) {
        eprintln!(
            "[{}ms] {message}",
            STARTED.get_or_init(Instant::now).elapsed().as_millis()
        );
    }
}
