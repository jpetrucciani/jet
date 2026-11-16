//! The window's taskbar button: progress from OSC 9;4.

use jet_core::Progress;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::{
    ITaskbarList3, TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPF_PAUSED,
    TaskbarList,
};

pub struct Taskbar {
    list: ITaskbarList3,
    shown: Progress,
}

impl Taskbar {
    /// Needs COM initialized on this thread.
    pub fn new() -> Option<Self> {
        let list: ITaskbarList3 =
            unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) }.ok()?;
        unsafe { list.HrInit() }.ok()?;
        Some(Self {
            list,
            shown: Progress::Hidden,
        })
    }

    /// Show `progress` on `hwnd`'s button, if it isn't already.
    pub fn show(&mut self, hwnd: HWND, progress: Progress) {
        if progress == self.shown {
            return;
        }
        let (state, value) = match progress {
            Progress::Hidden => (TBPF_NOPROGRESS, None),
            Progress::Normal(p) => (TBPF_NORMAL, Some(p)),
            Progress::Error(p) => (TBPF_ERROR, Some(p)),
            Progress::Paused(p) => (TBPF_PAUSED, Some(p)),
            Progress::Indeterminate => (TBPF_INDETERMINATE, None),
        };
        // Fails until the button exists (or never, without one); the next
        // change tries again.
        let mut ok = unsafe { self.list.SetProgressState(hwnd, state) }.is_ok();
        if let Some(p) = value {
            ok &= unsafe { self.list.SetProgressValue(hwnd, u64::from(p), 100) }.is_ok();
        }
        if ok {
            self.shown = progress;
        }
    }
}
