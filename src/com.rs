//! RAII wrapper for per-thread COM initialization.

use windows::Win32::{
    Foundation::{S_FALSE, S_OK},
    System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED},
};

/// Ensures `CoInitializeEx(..., COINIT_APARTMENTTHREADED)` is paired with
/// `CoUninitialize()` when dropped, avoiding unmatched COM reference counts.
pub(crate) struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    pub(crate) fn init_sta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        Self {
            should_uninit: hr == S_OK || hr == S_FALSE,
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ComGuard;

    #[test]
    fn com_guard_balanced_reentrant() {
        let outer = ComGuard::init_sta();
        {
            let inner = ComGuard::init_sta();
            assert!(inner.should_uninit || !outer.should_uninit);
        }
        drop(outer);
    }
}
