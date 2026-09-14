use std::sync::Mutex;

// This state is independent of diagnostics and the lazy flash worker. No
// Windows call, pixel read, UIA query or compositor wait holds its mutex.
pub(crate) static FLASH_COLOR_GATE: FlashColorGate = FlashColorGate::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FlashColorEpoch(u64);

impl FlashColorEpoch {
    pub(crate) fn follows_flash(self) -> bool {
        self.0 != 0
    }
}

struct State {
    epoch: u64,
    active: usize,
    blocked: bool,
}

impl State {
    fn changed(&mut self) {
        match self.epoch.checked_add(1) {
            Some(epoch) => self.epoch = epoch,
            None => self.blocked = true,
        }
    }
}

pub(crate) struct FlashColorGate {
    state: Mutex<State>,
}

impl FlashColorGate {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(State {
                epoch: 0,
                active: 0,
                blocked: false,
            }),
        }
    }

    pub(crate) fn idle_epoch(&self) -> Option<FlashColorEpoch> {
        let state = self.state.try_lock().ok()?;
        (state.active == 0 && !state.blocked).then_some(FlashColorEpoch(state.epoch))
    }

    pub(crate) fn begin(&self) -> FlashColorGuard<'_> {
        if let Ok(mut state) = self.state.lock() {
            state.changed();
            state.active = state.active.saturating_add(1);
        }
        FlashColorGuard {
            gate: self,
            removed: false,
        }
    }
}

// Install after creating the hidden HWND and before any show operation. The
// native owner marks removal only after DestroyWindow succeeds (or the HWND
// is already gone). An unconfirmed cleanup disables further color reads for
// this process instead of allowing a possibly visible flash into the cache.
pub(crate) struct FlashColorGuard<'a> {
    gate: &'a FlashColorGate,
    removed: bool,
}

impl FlashColorGuard<'_> {
    pub(crate) fn mark_removed(&mut self) {
        self.removed = true;
    }
}

impl Drop for FlashColorGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.active = state.active.saturating_sub(1);
            state.blocked |= !self.removed;
            state.changed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complete_flash_changes_the_epoch_even_if_both_observations_are_idle() {
        let gate = FlashColorGate::new();
        let before = gate.idle_epoch().unwrap();
        assert!(!before.follows_flash());
        let mut flash = gate.begin();
        assert!(gate.idle_epoch().is_none());
        flash.mark_removed();
        // Removal is published when the native owner's guard is dropped.
        assert!(gate.idle_epoch().is_none());
        drop(flash);
        let after = gate.idle_epoch().unwrap();
        assert_ne!(before, after);
        assert!(after.follows_flash());
    }

    #[test]
    fn overlapping_owners_keep_reads_blocked_until_both_are_removed() {
        let gate = FlashColorGate::new();
        let mut first = gate.begin();
        let mut second = gate.begin();
        first.mark_removed();
        drop(first);
        assert!(gate.idle_epoch().is_none());
        second.mark_removed();
        drop(second);
        assert!(gate.idle_epoch().is_some());
    }

    #[test]
    fn failed_native_cleanup_cannot_be_erased_by_a_later_successful_flash() {
        let gate = FlashColorGate::new();
        drop(gate.begin());
        assert!(gate.idle_epoch().is_none());
        let mut next = gate.begin();
        next.mark_removed();
        drop(next);
        assert!(gate.idle_epoch().is_none());
    }

    #[test]
    fn a_busy_gate_is_skipped_without_waiting_for_the_writer() {
        let gate = FlashColorGate::new();
        let state = gate.state.lock().unwrap();
        assert!(gate.idle_epoch().is_none());
        drop(state);
        assert!(gate.idle_epoch().is_some());
    }

    #[test]
    fn epoch_exhaustion_fails_closed_instead_of_reusing_an_old_token() {
        let gate = FlashColorGate::new();
        gate.state.lock().unwrap().epoch = u64::MAX;
        let mut flash = gate.begin();
        flash.mark_removed();
        drop(flash);
        assert!(gate.idle_epoch().is_none());
    }
}
