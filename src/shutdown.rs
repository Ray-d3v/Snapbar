use std::thread;

pub fn defer_cleanup(name: &'static str, cleanup: impl FnOnce() + Send + 'static) {
    if let Err(error) = thread::Builder::new().name(name.to_string()).spawn(cleanup) {
        eprintln!("{name}を開始できませんでした: {error}");
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, thread, time::Duration};

    use super::defer_cleanup;

    const TIMEOUT: Duration = Duration::from_secs(2);

    #[test]
    fn deferred_cleanup_does_not_block_its_caller() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let (returned_tx, returned_rx) = mpsc::channel();

        let caller = thread::spawn(move || {
            defer_cleanup("snapbar-test-cleanup", move || {
                entered_tx.send(()).expect("cleanup should start");
                release_rx.recv().expect("cleanup should be released");
                finished_tx.send(()).expect("cleanup should finish");
            });
            returned_tx.send(()).expect("caller should return");
        });

        entered_rx
            .recv_timeout(TIMEOUT)
            .expect("cleanup should run on its worker");
        returned_rx
            .recv_timeout(TIMEOUT)
            .expect("caller should not wait for cleanup completion");
        release_tx.send(()).expect("cleanup should be released");
        finished_rx
            .recv_timeout(TIMEOUT)
            .expect("cleanup should finish after release");
        caller.join().expect("caller should not panic");
    }
}
