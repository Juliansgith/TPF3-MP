//! Platform entry points that get [`crate::bootstrap`] running early, off the
//! loader lock.

#![allow(unsafe_code)]

#[cfg(windows)]
mod windows {
    use core::ffi::c_void;

    use windows_sys::Win32::Foundation::{CloseHandle, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
    use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
    use windows_sys::Win32::System::Threading::CreateThread;

    /// `TRUE`, the value `DllMain` returns to let the load proceed.
    const DLL_MAIN_OK: i32 = 1;

    /// The DLL entry point. On attach it starts a worker thread and returns at
    /// once: doing the real work here would run under the loader lock, and
    /// loading a library or blocking there can deadlock the process.
    #[unsafe(no_mangle)]
    pub extern "system" fn DllMain(module: HMODULE, reason: u32, _reserved: *mut c_void) -> i32 {
        if reason == DLL_PROCESS_ATTACH {
            // SAFETY: DisableThreadLibraryCalls just drops thread notifications
            // for our module; the handle is the one the loader passed us.
            unsafe {
                DisableThreadLibraryCalls(module);
            }
            // SAFETY: create a plain worker thread; `bootstrap_thread` has the
            // required signature and does not touch the loader lock.
            let handle = unsafe {
                CreateThread(
                    core::ptr::null(),
                    0,
                    Some(bootstrap_thread),
                    core::ptr::null(),
                    0,
                    core::ptr::null_mut(),
                )
            };
            if !handle.is_null() {
                // SAFETY: the thread runs detached; we only drop our handle.
                unsafe {
                    CloseHandle(handle);
                }
            }
        }
        DLL_MAIN_OK
    }

    extern "system" fn bootstrap_thread(_parameter: *mut c_void) -> u32 {
        crate::bootstrap();
        0
    }
}

#[cfg(unix)]
mod unix {
    /// A load-time constructor: on Linux via `LD_PRELOAD`, on macOS via the
    /// bundled dylib. It hands off to a thread so the game's own startup is
    /// never blocked by our work. `ctor` runs this before `main`, which is
    /// inherently unsafe, hence `#[ctor(unsafe)]`.
    #[ctor::ctor(unsafe)]
    fn tpf3mp_hook_init() {
        std::thread::spawn(crate::bootstrap);
    }
}
