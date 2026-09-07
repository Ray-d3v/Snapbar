use std::marker::PhantomData;
use std::rc::Rc;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
};

/// Keep DWM, UIA and Win32 coordinates in physical pixels. Never send this guard
/// across threads: Windows' previous awareness context belongs to its creator.
pub(crate) struct PhysicalPixels {
    previous: DPI_AWARENESS_CONTEXT,
    _thread: PhantomData<Rc<()>>,
}

impl PhysicalPixels {
    pub(crate) fn enter() -> Self {
        Self {
            previous: unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            },
            _thread: PhantomData,
        }
    }
}

impl Drop for PhysicalPixels {
    fn drop(&mut self) {
        if !self.previous.0.is_null() {
            unsafe {
                SetThreadDpiAwarenessContext(self.previous);
            }
        }
    }
}
