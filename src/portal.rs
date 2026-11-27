//! GNOME/KDE portal session. Capture and input must share this Arc; do not
//! recreate it in an encoder subprocess (that would request consent again).
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortalError { pub code: i32 }

impl std::fmt::Display for PortalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", error(self.code))
    }
}

impl std::error::Error for PortalError {}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FrameVersion(usize, u64);

#[repr(C)]
#[derive(Default)]
pub struct DmaFrame {
    pub fd: i32,
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub offset: u32,
    pub size: u32,
    pub modifier: u64,
}
pub enum DmaCapture<T> {
    Frame(T),
    Pending,
    Cpu,
    /// Stream or handle closed/ended while DMA-BUF was otherwise eligible: run dead-stream
    /// blank/recovery only, never switch a freshly installed live handle to SHM.
    Dead,
    Blank,
    /// Capture dimensions changed before or during DMA-BUF acquire: restart encoder without
    /// disabling DMA-BUF.
    SizeChanged,
    /// DMA-BUF unavailable on the optional handle id (`None` = host/plugin-wide).
    Unavailable(i32, Option<u64>),
}

/// Fresh (changed) frames delivered by the current portal capture; reset only when a handle is
/// installed and bumped by both capture paths (CPU `capture_changed` and the native DMA-BUF
/// one) so every encoder — libx264, VAAPI, NVENC — publishes readiness. Discarded candidate
/// opens leave the installed session's proof intact. Privacy auto waits for `> 0` as proof the
/// capture produces content before hiding the screen.
static FRESH_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Fresh frames of the current portal capture; see `FRESH_FRAMES`.
pub fn fresh_frames() -> u64 { FRESH_FRAMES.load(std::sync::atomic::Ordering::Relaxed) }
/// Restart the proof only when a handle becomes the installed capture source. A candidate open
/// that is later discarded must not erase the old session's evidence.
fn mark_installed_capture() { FRESH_FRAMES.store(0, std::sync::atomic::Ordering::Relaxed); }
/// A frame counts toward privacy only while its session is the installed capture source.
fn note_capture_frame(installed: bool, frames: &std::sync::atomic::AtomicU64) {
    if installed { frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
}
#[cfg(unix)]
#[path = "portal_permissions.rs"]
mod permissions;

#[cfg(all(target_os = "linux", feature = "portal"))]
mod native {
    use std::{ffi::c_void, ptr::NonNull, sync::{Arc, Mutex, MutexGuard, TryLockError}};
    crate::engine_ffi! { crate::engine_plugin::Engine::Portal;
        fn uu_portal_open_persist(restore: *const libc::c_char, save: extern "C" fn(*const libc::c_char, *mut c_void), data: *mut c_void, error: *mut i32, width: *mut u32, height: *mut u32) -> *mut c_void;
        fn uu_portal_open_virtual(restore: *const libc::c_char, save: extern "C" fn(*const libc::c_char, *mut c_void), data: *mut c_void, error: *mut i32, width: *mut u32, height: *mut u32) -> *mut c_void;
        fn uu_portal_open_persist_cancellable(restore: *const libc::c_char, save: extern "C" fn(*const libc::c_char, *mut c_void), save_data: *mut c_void, cancelled: extern "C" fn(*mut c_void) -> i32, cancel_data: *mut c_void, error: *mut i32, width: *mut u32, height: *mut u32) -> *mut c_void;
        fn uu_portal_open_persist_capture(restore: *const libc::c_char, save: extern "C" fn(*const libc::c_char, *mut c_void), save_data: *mut c_void, cancelled: Option<extern "C" fn(*mut c_void) -> i32>, cancel_data: *mut c_void, prefer_dmabuf: i32, error: *mut i32, width: *mut u32, height: *mut u32) -> *mut c_void;
        fn uu_portal_open_detailed(restore: *const libc::c_char, save: extern "C" fn(*const libc::c_char, *mut c_void, i32, i32, i32), save_data: *mut c_void, cancelled: Option<extern "C" fn(*mut c_void) -> i32>, cancel_data: *mut c_void, prefer_dmabuf: i32, source_types: u32, error: *mut i32, width: *mut u32, height: *mut u32) -> *mut c_void;
        fn uu_portal_set_cursor_mode(mode: i32);
        fn uu_portal_cursor_mode(portal: *mut c_void) -> i32;
        fn uu_portal_cursor_modes(portal: *mut c_void) -> i32;
        fn uu_portal_clipboard_supported(portal: *mut c_void) -> i32;
        fn uu_portal_clipboard_ready(portal: *mut c_void) -> i32;
        fn uu_portal_clipboard_set(portal: *mut c_void, text: *const u8, length: usize) -> i32;
        fn uu_portal_clipboard_copy(from: *mut c_void, to: *mut c_void) -> i32;
        fn uu_portal_clipboard_poll(portal: *mut c_void, text: *mut u8, capacity: usize) -> i32;
        fn uu_portal_close(portal: *mut c_void);
        fn uu_portal_alive(portal: *mut c_void) -> i32;
        fn uu_portal_restore_sent() -> i32;
        fn uu_portal_capture_alive(portal: *mut c_void) -> i32;
        fn uu_portal_wait_frame_since(portal: *mut c_void, seen: *mut u64, timeout_us: i64) -> i32;
        fn uu_portal_wait_frame(portal: *mut c_void, timeout_us: i64) -> i32;
        fn uu_portal_input_dispatch(portal: *mut c_void);
        fn uu_portal_reset_frame(portal: *mut c_void);
        fn uu_portal_capture_since(portal: *mut c_void, bgra: *mut u8, length: usize, revision: *mut u64) -> i32;
        fn uu_portal_dmabuf_acquire(portal: *mut c_void, frame: *mut super::DmaFrame, revision: *mut u64) -> i32;
        fn uu_portal_dmabuf_release(portal: *mut c_void);
        fn uu_portal_dmabuf_abandon(portal: *mut c_void);
        fn uu_portal_dmabuf_disable(portal: *mut c_void);
        fn uu_portal_move(portal: *mut c_void, x: f64, y: f64, relative: i32) -> i32;
        fn uu_portal_button(portal: *mut c_void, button: i32, pressed: i32) -> i32;
        fn uu_portal_key(portal: *mut c_void, keysym: u32, keycode: i32, pressed: i32) -> i32;
        fn uu_portal_scroll(portal: *mut c_void, dx: f64, dy: f64, finish: i32) -> i32;
        fn uu_portal_touch_supported(portal: *mut c_void) -> i32;
        fn uu_portal_touch_available(portal: *mut c_void) -> i32;
        fn uu_portal_touch(portal: *mut c_void, phase: i32, x: f64, y: f64) -> i32;
        fn uu_portal_stream_position(portal: *mut c_void, x: *mut i32, y: *mut i32) -> i32;
        fn uu_portal_monitors(out: *mut libc::c_char, length: usize) -> i32;
        fn uu_portal_end(portal: *mut c_void);
    }
    static NEXT_HANDLE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    struct Handle(NonNull<c_void>, u64);
    impl Handle {
        fn new(ptr: NonNull<c_void>) -> Self {
            Self(ptr, NEXT_HANDLE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
        }
    }
    fn pack(width: u32, height: u32) -> u64 { (u64::from(width) << 32) | u64::from(height) }
    // Calls are serialized by Session's mutex except wait_frame, whose C-side
    // condition wait synchronizes with callbacks and input through the engine lock.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}
    impl Drop for Handle {
        fn drop(&mut self) {
            #[cfg(test)]
            if self.0 == NonNull::dangling() { return; }
            unsafe { uu_portal_close(self.0.as_ptr()) }
        }
    }
    fn lease_handle<T>(current: &Mutex<Arc<T>>) -> Result<Arc<T>, String> {
        current.lock().map(|handle| Arc::clone(&handle)).map_err(|_| "Portal session lock poisoned".into())
    }
    fn blank_changed(frame: &mut [u8], version: &mut Option<super::FrameVersion>) -> bool {
        if *version == Some(super::FrameVersion(0, 0)) { return false; }
        super::blank(frame); *version = Some(super::FrameVersion(0, 0)); true
    }
    // Portal preparation and native teardown can wait for consent or D-Bus.
    // Only the swap may hold the mutex shared with input and clipboard.
    // Only the unit tests swap a test handle now; production swaps carry the clipboard.
    #[cfg(test)]
    fn replace_handle<T>(current: &Mutex<T>, locked: MutexGuard<'_, T>, prepare: impl FnOnce() -> Result<Option<T>, String>) -> Result<bool, String> {
        drop(locked);
        let Some(replacement) = prepare()? else { return Ok(false); };
        let old = {
            let mut handle = current.lock().map_err(|_| "Portal session lock poisoned")?;
            std::mem::replace(&mut *handle, replacement)
        };
        drop(old);
        Ok(true)
    }
    /// Swap `replacement` into `current`, carrying clipboard state and clearing held portal input.
    /// `still_valid` and `on_installed` run under `current`'s mutex; if `still_valid` returns false,
    /// all locks (`current`, input held, clipboard) are released BEFORE dropping `replacement` so
    /// synchronous D-Bus teardown cannot stall input releases or clipboard writers.
    fn replace_handle_guarded<H>(
        current: &Mutex<Arc<H>>,
        clipboard_write: &Mutex<()>,
        locked: MutexGuard<'_, Arc<H>>,
        prepare: impl FnOnce() -> Result<Option<Arc<H>>, String>,
        still_valid: impl FnOnce() -> bool,
        on_installed: impl FnOnce(),
        copy_clipboard: impl FnOnce(&H, &H),
        on_replaced: impl FnOnce(),
    ) -> Result<bool, String> {
        drop(locked);
        let Some(replacement) = prepare()? else { return Ok(false); };
        let clipboard = clipboard_write.lock().map_err(|_| "Portal clipboard lock poisoned")?;
        let serial = crate::input::lock_portal_held();
        let mut handle = current.lock().map_err(|_| "Portal session lock poisoned")?;
        if !still_valid() {
            drop(handle);
            drop(serial);
            drop(clipboard);
            drop(replacement);
            return Ok(false);
        }
        let old = std::mem::replace(&mut *handle, replacement);
        let installed = Arc::clone(&*handle);
        on_installed();
        drop(handle);
        copy_clipboard(&old, &installed);
        drop(clipboard);
        on_replaced();
        drop(serial);
        drop(old);
        Ok(true)
    }
    /// Like `replace_handle`, but carries the viewer's clipboard text before the old session
    /// closes. Clipboard writers wait from before the swap until the carry-over finishes.
    fn replace_handle_carrying(current: &Mutex<Arc<Handle>>, clipboard_write: &Mutex<()>, locked: MutexGuard<'_, Arc<Handle>>, prepare: impl FnOnce() -> Result<Option<Arc<Handle>>, String>, still_valid: impl FnOnce() -> bool, on_installed: impl FnOnce()) -> Result<bool, String> {
        replace_handle_guarded(
            current,
            clipboard_write,
            locked,
            prepare,
            still_valid,
            on_installed,
            |old, installed| unsafe { uu_portal_clipboard_copy(old.0.as_ptr(), installed.0.as_ptr()); },
            || crate::input::portal_session_replaced(),
        )
    }
    #[derive(Default)]
    struct Recovery { retry_at: Option<std::time::Instant>, failed_until: Option<std::time::Instant>, denied: bool }
    impl Recovery {
        // Shared by the encoder's stream-loss recovery and the desktop supervisor: a failed open
        // backs both off, and a terminal consent/format failure stops the encoder from prompting again.
        fn failed(&mut self, now: std::time::Instant, code: i32) {
            let until = now + std::time::Duration::from_secs(5);
            self.failed_until = Some(until);
            self.retry_at = Some(until);
            if matches!(code, 4 | 5 | 6 | 8 | 14) { self.denied = true; }
        }
        fn succeeded(&mut self) { *self = Self::default(); }
        fn ready(&mut self, now: std::time::Instant, unlocked: impl FnOnce() -> bool) -> bool {
            if self.denied { return false; }
            let deadline = self.retry_at.get_or_insert(now + std::time::Duration::from_secs(1));
            if now < *deadline { return false; }
            *deadline = now + std::time::Duration::from_secs(1);
            unlocked()
        }
    }
    fn clear_pause_for_unlocked_replacement(was_locked: &std::sync::atomic::AtomicBool, state: Option<bool>) {
        if state == Some(false) {
            was_locked.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    pub struct Session { handle: Mutex<Arc<Handle>>, locked: tokio::sync::watch::Receiver<Option<bool>>, was_locked: std::sync::atomic::AtomicBool, recovery: Mutex<Recovery>, size: std::sync::atomic::AtomicU64, monitor: Mutex<Option<String>>, virtual_output: std::sync::atomic::AtomicBool, geometry_pending: std::sync::atomic::AtomicBool, clipboard_write: Mutex<()>, end_generation: Arc<std::sync::atomic::AtomicU64>, ended: std::sync::atomic::AtomicBool, dma_failed: std::sync::atomic::AtomicBool, dma_shm_only: std::sync::atomic::AtomicBool, dma_shm_handle: std::sync::atomic::AtomicU64, installed: std::sync::atomic::AtomicBool }
    /// Whether a failed open means the portal has already seen (and therefore spent) the
    /// single-use restore token. Codes 1 (no bus), 2 (no interface) and 8 (allocation) fail
    /// inside the engine before anything is sent, and 13 is this side giving up (screen locked,
    /// viewer gone). In all of those the cached grant is still valid, while a token-less retry
    /// would put a consent dialog on an unattended screen. Only a fallback for engines that
    /// predate `uu_portal_restore_sent`: code 5 (timeout) and 3 (invalid reply) also happen at
    /// CreateSession, before the token is ever submitted.
    fn token_spent(code: i32) -> bool { !matches!(code, 1 | 2 | 8 | 13) }

    /// Whether a failed open leaves the cached restore grant valid. `submitted` is the engine's
    /// word on whether SelectDevices carried the token; `None` means an old engine that cannot
    /// report it, falling back to the error code.
    fn keep_restore_grant(submitted: Option<bool>, code: i32) -> bool {
        match submitted { Some(sent) => !sent, None => !token_spent(code) }
    }

    #[cfg(test)]
    mod restore_grant_tests {
        use super::keep_restore_grant;
        // The regression Codex measured: CreateSession (portal_engine.c) answers 5 (timeout)
        // or 3 (invalid session handle) before SelectDevices ever carries the token, so the
        // two-strike code rule discarded a grant the portal had never seen. The engine's
        // submission flag keeps it.
        #[test] fn create_session_timeout_keeps_the_cached_grant() {
            for code in [3, 5, 13] { assert!(keep_restore_grant(Some(false), code), "code {code} before submission keeps the grant"); }
        }
        #[test] fn cancellation_before_dbus_submission_keeps_the_cached_grant() {
            assert!(keep_restore_grant(Some(false), 13), "a cancel before request() submits the token must keep the grant");
        }
        // Once SelectDevices carried the token the portal may have consumed it: discard and
        // retry without, regardless of code — including the cancels that land mid-dialog.
        #[test] fn submitted_failures_discard_the_grant() {
            for code in [0, 3, 4, 5, 9, 13, 15] { assert!(!keep_restore_grant(Some(true), code), "code {code} after submission spent the token"); }
        }
        // Engines without uu_portal_restore_sent keep the previous code-based rule:
        // 1 bus, 2 interface, 8 allocation never reached the portal; the rest are treated as spent.
        #[test] fn unknown_engines_fall_back_to_codes() {
            for code in [1, 2, 8, 13] { assert!(keep_restore_grant(None, code), "code {code} never reached the portal"); }
            for code in [0, 3, 4, 5, 9, 15] { assert!(!keep_restore_grant(None, code), "code {code} assumed at the portal"); }
        }
    }

    impl Session {
        fn install_capture_proof(&self) {
            self.installed.store(true, std::sync::atomic::Ordering::Release);
            super::mark_installed_capture();
        }
        fn note_frame(&self) {
            super::note_capture_frame(self.installed.load(std::sync::atomic::Ordering::Acquire), &super::FRESH_FRAMES);
        }
        fn take_lock_observation(&self) -> Result<bool, String> {
            // A recovery in progress must see the lock even if another capture
            // observes unlock and would otherwise reset only the old handle.
            let _recovery = match self.recovery.try_lock() {
                Ok(recovery) => recovery,
                Err(TryLockError::WouldBlock) => return Ok(false),
                Err(TryLockError::Poisoned(_)) => return Err("Portal recovery lock poisoned".into()),
            };
            Ok(self.was_locked.swap(false, std::sync::atomic::Ordering::AcqRel))
        }
        fn check_geometry(&self, replacement: &Self) -> Result<(), super::PortalError> {
            // Encoder buffers use frame pixels. Logical geometry and EIS regions
            // belong to the replacement handle and may change with scaling (#25).
            if self.size() != replacement.size() {
                Err(super::PortalError { code: 14 })
            } else {
                Ok(())
            }
        }
        /// Verify geometry for a recovery replacement: normally dimensions must match the running
        /// pipeline, except after an idle privacy-screen exit (`geometry_pending == true`), where
        /// the next reopen switches back from the virtual output to the physical monitor's size.
        fn check_recovery_geometry(&self, replacement: &Self) -> Result<Option<(u32, u32)>, super::PortalError> {
            if self.geometry_pending.load(std::sync::atomic::Ordering::Acquire) {
                return Ok(Some(replacement.size()));
            }
            self.check_geometry(replacement)?;
            Ok(None)
        }
        fn allow_dmabuf(&self) -> bool {
            !self.dma_failed.load(std::sync::atomic::Ordering::Acquire)
                && !self.dma_shm_only.load(std::sync::atomic::Ordering::Acquire)
        }
        /// Swap `replacement` in, carrying the viewer's clipboard text before the old session
        /// closes. Every caller passes the `expected` generation sampled before opening so a
        /// desktop end that raced the open always rejects installation and drops the candidate
        /// outside the handle, input and clipboard locks.
        fn install_replacement(&self, replacement: Self, expected: u64, defer_held: bool, refresh_privacy: bool) -> Result<(u32, u32), String> {
            let size = replacement.size();
            let next = replacement.handle.into_inner().map_err(|_| "Portal session lock poisoned".to_string())?;
            let clipboard = self.clipboard_write.lock().map_err(|_| "Portal clipboard lock poisoned".to_string())?;
            // Records made against the new handle must postdate the reset below.
            let serial = crate::input::lock_portal_held();
            // Checked under the serial lock: an input recorded after an earlier check would
            // otherwise be destroyed with the session it was sent to.
            if defer_held && crate::input::portal_input_held() {
                drop(serial);
                drop(clipboard);
                drop(next);
                return Err("input held during the swap".into());
            }
            let mut handle = self.handle.lock().map_err(|_| "Portal session lock poisoned".to_string())?;
            if self.ended.load(std::sync::atomic::Ordering::Acquire)
                || self.end_generation.load(std::sync::atomic::Ordering::Acquire) != expected
            {
                drop(handle);
                drop(serial);
                drop(clipboard);
                drop(next);
                return Err("desktop session ended during the rebuild".into());
            }
            self.size.store(pack(size.0, size.1), std::sync::atomic::Ordering::Release);
            self.geometry_pending.store(false, std::sync::atomic::Ordering::Release);
            let previous_handle = std::mem::replace(&mut *handle, next);
            let installed = Arc::clone(&*handle);
            let shm_handle = if self.dma_shm_only.load(std::sync::atomic::Ordering::Acquire) {
                installed.1
            } else { 0 };
            self.dma_shm_handle.store(shm_handle, std::sync::atomic::Ordering::Release);
            // Under the handle mutex: a capture publishes its frame proof while holding that
            // lock, so the reset cannot erase the first frame taken after the swap.
            self.install_capture_proof();
            drop(handle);
            #[cfg(test)]
            let skip_ffi = previous_handle.0 == NonNull::dangling() || installed.0 == NonNull::dangling();
            #[cfg(not(test))]
            let skip_ffi = false;
            if !skip_ffi && !self.allow_dmabuf() {
                unsafe { uu_portal_dmabuf_disable(installed.0.as_ptr()) };
            }
            if !skip_ffi && unsafe { uu_portal_clipboard_copy(previous_handle.0.as_ptr(), installed.0.as_ptr()) } < 0 {
                log::warn!("Portal clipboard ownership could not be carried over");
            }
            drop(clipboard);
            // The replaced devices took any held portal input with them.
            crate::input::portal_session_replaced();
            drop(serial);
            drop(previous_handle);
            // A replaced virtual output can move in the layout; absolute input needs its new share.
            if refresh_privacy { crate::privacy_screen::refresh_pointer_region(); }
            Ok(size)
        }
        /// Point the host and the engine at the cursor mode the installed session really has.
        fn restore_installed_cursor_mode(&self, fallback: bool) {
            let installed = self.handle.lock().ok().map(|handle| unsafe { uu_portal_cursor_mode(handle.0.as_ptr()) }).unwrap_or(0);
            let embedded = if installed > 0 { installed == 2 } else { fallback };
            unsafe { uu_portal_set_cursor_mode(if embedded { 2 } else { 1 }); }
            super::CURSOR_MODE.store(if embedded { 2 } else { 1 }, std::sync::atomic::Ordering::Release);
        }
        pub fn is_alive(&self) -> bool {
            self.handle.lock().is_ok_and(|handle| {
                #[cfg(test)]
                if handle.0 == NonNull::dangling() { return false; }
                unsafe { uu_portal_alive(handle.0.as_ptr()) != 0 }
            })
        }
        /// True only while frames actually flow. `uu_portal_alive` stays true after the PipeWire
        /// stream has been interrupted past its deadline, and a failed reopen is held back for a
        /// few seconds by `recovery`; both mean no picture, so a caller that keeps the host awake
        /// or schedules a reopen must not read them as healthy.
        pub fn capture_healthy(&self) -> bool {
            let alive = self.handle.lock().is_ok_and(|handle| unsafe {
                uu_portal_alive(handle.0.as_ptr()) != 0 && uu_portal_capture_alive(handle.0.as_ptr()) != 0
            });
            alive && !self.recovery_failed()
        }
        fn recovery_failed(&self) -> bool {
            // Called from async health checks: a blocking lock here would stall the whole
            // supervisor for as long as an in-progress consent dialog can wait (upgrade_touch
            // holds this mutex across its open). A held lock means an open or recovery is
            // running, not that recovery failed - the stream aliveness read beside it stays
            // the judge of health until that operation reports its outcome.
            match self.recovery.try_lock() {
                Ok(recovery) => recovery.failed_until.is_some_and(|until| std::time::Instant::now() < until),
                Err(TryLockError::WouldBlock | TryLockError::Poisoned(_)) => false,
            }
        }
        /// Opens a session for `monitor` (a Mutter connector) with that monitor's saved consent;
        /// None means the screen currently selected, or the only monitor.
        /// `fresh` asks the host again instead of restoring the saved consent (which stays saved).
        /// `virtual_output` shares a KWin virtual output instead (privacy screen) with its own consent.
        fn open_inner(cancelled: Option<&dyn Fn() -> bool>, monitor: Option<&str>, fresh: bool, virtual_output: bool, allow_dmabuf: bool) -> Result<Self, super::PortalError> {
            struct CancelCheck<'a>(&'a dyn Fn() -> bool);
            extern "C" fn is_cancelled(data: *mut c_void) -> i32 {
                let check = unsafe { &*(data as *const CancelCheck<'_>) };
                i32::from((check.0)())
            }
            if cancelled.is_some_and(|check| check()) {
                return Err(super::PortalError { code: 13 });
            }
            let cancel_check = cancelled.map(CancelCheck);
            // engine-plugins: libuu-portal.so (or one of its libraries) may be missing on this host.
            crate::engine_plugin::load(crate::engine_plugin::Engine::Portal).map_err(|_| super::PortalError { code: 15 })?;
            // Release packages can temporarily have an older libuu-portal.so than the host.
            #[cfg(feature = "engine-plugins")]
            let detailed = crate::engine_plugin::symbol(crate::engine_plugin::Engine::Portal, "uu_portal_open_detailed\0") != 0;
            #[cfg(not(feature = "engine-plugins"))]
            let detailed = true;
            // Legacy plugins without uu_portal_open_detailed only have uncancellable
            // uu_portal_open_virtual; refuse a cancellable virtual open before borrowing the
            // restore token or global OPENING lock rather than blocking for the 120 s timeout.
            if virtual_output && !detailed && cancelled.is_some() {
                return Err(super::PortalError { code: 2 });
            }
            let directory = super::permissions::directory();
            let key = super::permissions::desktop_key();
            let wanted = if virtual_output { None } else { monitor.map(str::to_owned).or_else(crate::wayland::portal_monitor) };
            let wanted_key = wanted.as_deref().map(super::permissions::monitor_key);
            let virtual_key = format!("{key}-virtual");
            // One opener at a time: consent tokens are single-use, and two concurrent openers
            // (privacy virtual-output switch and closure recovery after a quick reconnect) each
            // consumed one and both failed, leaving no saved consent (seen live, KDE).
            static OPENING: Mutex<()> = Mutex::new(());
            let _opening = OPENING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // Each monitor has its own consent; the desktop-wide key is the single-monitor one.
            let taken = directory.as_deref().filter(|_| !fresh).and_then(|dir| {
                let take = |k: String| super::permissions::take(dir, &k).map(|v| (k, v));
                if virtual_output { return take(virtual_key.clone()); }
                wanted_key.as_ref().and_then(|name| take(super::permissions::pending_key(name)).or_else(|| take(name.clone())))
                    .or_else(|| take(super::permissions::pending_key(key))).or_else(|| take(key.to_owned()))
            });
            let token = taken.as_ref().and_then(|(_, value)| std::ffi::CString::new(value.as_str()).ok());
            let requested_key = if virtual_output { &virtual_key } else { wanted_key.as_deref().unwrap_or(key) };
            struct TokenCache { rotation: super::permissions::Rotation, virtual_key: Option<String>, fresh: bool }
            let mut cache = TokenCache {
                rotation: super::permissions::Rotation::new(directory.clone(), requested_key),
                virtual_key: virtual_output.then(|| virtual_key.clone()), fresh,
            };
            extern "C" fn save_detailed(token: *const libc::c_char, data: *mut c_void, has_position: i32, x: i32, y: i32) {
                let result = std::panic::catch_unwind(|| unsafe {
                    let cache = &mut *(data as *mut TokenCache);
                    if let Ok(token) = std::ffi::CStr::from_ptr(token).to_str() {
                        if cache.fresh { cache.rotation.remember(token); return; }
                        let granted = cache.virtual_key.clone().or_else(||
                            (has_position != 0).then(|| crate::wayland::portal_monitor_at(x, y)).flatten()
                                .map(|name| super::permissions::monitor_key(&name)));
                        if cache.rotation.record(token, granted.as_deref()).is_err() { log::warn!("Portal permission cache could not be saved"); }
                    }
                });
                if result.is_err() { log::warn!("Portal permission cache callback failed"); }
            }
            // Older plugins lack the position-aware entry point; use the pending slot.
            extern "C" fn save_token(token: *const libc::c_char, data: *mut c_void) {
                save_detailed(token, data, 0, 0, 0);
            }
            #[allow(unused_mut)]
            let mut prefer_dmabuf = false;
            #[cfg(feature = "inproc-av")]
            if allow_dmabuf && !virtual_output && Self::dmabuf_abandon_supported()
                && crate::video::native_engine_eligible(crate::video_select::selection().encoder, crate::video::NativeDisplayKind::Portal)
                && !std::env::var("UU_PORTAL_DMABUF").is_ok_and(|v| v == "0") {
                prefer_dmabuf = true;
            }
            #[cfg(not(feature = "inproc-av"))]
            let _ = allow_dmabuf;
            // One engine attempt with a given restore token. Reusable because a rejected token
            // needs an immediate second attempt without one.
            macro_rules! engine_open {
                ($restore:expr) => {{
                    let (mut error, mut width, mut height) = (0, 0, 0);
                    let pointer = if detailed {
                        // Virtual shares can wait for consent too; cancel them like monitor opens.
                        let callback = cancel_check.as_ref().map(|_| is_cancelled as extern "C" fn(*mut c_void) -> i32);
                        let cancel_data = cancel_check.as_ref().map_or(std::ptr::null_mut(), |value| value as *const CancelCheck<'_> as *mut c_void);
                        unsafe { uu_portal_open_detailed(
                            $restore, save_detailed, &mut cache as *mut _ as *mut c_void, callback, cancel_data,
                            i32::from(prefer_dmabuf), if virtual_output { 4 } else { 1 },
                            &mut error, &mut width, &mut height,
                        ) }
                    } else {
                        #[allow(unused_mut)]
                        let mut pointer: *mut c_void = std::ptr::null_mut();
                        #[allow(unused_mut)]
                        let mut dmabuf_failed = false;
                        #[cfg(feature = "inproc-av")]
                        if prefer_dmabuf {
                            let callback = cancel_check.as_ref().map(|_| is_cancelled as extern "C" fn(*mut c_void) -> i32);
                            let cancel_data = cancel_check.as_ref().map_or(std::ptr::null_mut(), |value| value as *const CancelCheck<'_> as *mut c_void);
                            pointer = unsafe { uu_portal_open_persist_capture(
                                $restore, save_token, &mut cache as *mut _ as *mut c_void, callback, cancel_data, 1,
                                &mut error, &mut width, &mut height,
                            ) };
                            if pointer.is_null() && error != 0 { dmabuf_failed = true; }
                            if pointer.is_null() && !dmabuf_failed { log::warn!("Portal engine lacks DMA-BUF initialization; falling back to legacy initialization"); }
                        }
                        if !dmabuf_failed && pointer.is_null() { unsafe {
                            if virtual_output {
                                uu_portal_open_virtual($restore, save_token, &mut cache as *mut _ as *mut c_void, &mut error, &mut width, &mut height)
                            } else if let Some(check) = cancel_check.as_ref() {
                                uu_portal_open_persist_cancellable($restore, save_token, &mut cache as *mut _ as *mut c_void,
                                    is_cancelled, check as *const CancelCheck<'_> as *mut c_void, &mut error, &mut width, &mut height)
                            } else {
                                uu_portal_open_persist($restore, save_token, &mut cache as *mut _ as *mut c_void, &mut error, &mut width, &mut height)
                            }
                        } } else { pointer }
                    };
                    (pointer, error, width, height)
                }};
            }
            let mut restore = token.as_ref().map_or(std::ptr::null(), |value| value.as_ptr());
            let failed = |code: i32, cache: &TokenCache, save_grant: bool| {
                // A single-use restore token is consumed by the portal when it validates it, so a
                // rejection (expiry, permission-store reset, rotation drift) leaves no valid grant.
                // Writing the dead token back poisons every later attempt with an instant rejection
                // and no dialog - no picture, nothing to click, an unattended host bricks itself
                // (seen live on GNOME). But a portal that refuses every session outright is not the
                // token's fault; only the caller knows which case this was.
                if let (Some(dir), Some((k, value)), false, true) = (directory.as_deref(), taken.as_ref(), cache.rotation.received(), save_grant) {
                    if super::permissions::save(dir, k, value).is_err() { log::warn!("Portal permission cache could not be restored"); }
                }
                super::PortalError { code }
            };
            // Release packages can ship an older libuu-portal.so without the submission flag;
            // then the error code is all we have.
            #[cfg(feature = "engine-plugins")]
            let reports_submission = crate::engine_plugin::symbol(crate::engine_plugin::Engine::Portal, "uu_portal_restore_sent\0") != 0;
            #[cfg(not(feature = "engine-plugins"))]
            let reports_submission = true;
            let (mut pointer, mut error, mut width, mut height) = engine_open!(restore);
            // Self-heal a dead consent: retry once without the restore token so the portal pops
            // its consent dialog and issues a fresh one. Failures after Start carry a returned
            // token (rotation.received()), which excludes them from this retry.
            if pointer.is_null() && !restore.is_null() && !cache.rotation.received() {
                let submitted = reports_submission.then(|| unsafe { uu_portal_restore_sent() != 0 });
                if !keep_restore_grant(submitted, error) {
                    log::warn!("Portal request failed with a restore token; discarding it and retrying once without restore (a consent dialog may appear)");
                    restore = std::ptr::null();
                    (pointer, error, width, height) = engine_open!(restore);
                } else {
                    // Nothing was submitted, so the grant this attempt borrowed is still good.
                    // Put it back rather than trading a valid consent for a dialog nobody can see.
                    log::warn!("Portal request failed with code {error} before the restore token was submitted; keeping the cached consent");
                    return Err(failed(error, &cache, true));
                }
            }
            let Some(handle) = NonNull::new(pointer) else {
                // The retry carried no token, so its outcome tells the two failure shapes apart:
                // a fast code 3 means the portal refuses every session (seen wedged live on GNOME
                // after heavy session churn) - keep the grant, a portal restart will then restore
                // silently. A consent timeout (5) or cancel (13) means the dialog did open, i.e.
                // the portal is healthy and the token was genuinely dead: leave it discarded.
                let wedged = restore.is_null() && taken.is_some() && error == 3;
                return Err(failed(if error == 0 { 15 } else { error }, &cache, wedged));
            };
            let (mut x, mut y) = (0, 0);
            let granted = (!virtual_output && unsafe { uu_portal_stream_position(handle.as_ptr(), &mut x, &mut y) } == 0)
                .then(|| crate::wayland::portal_monitor_at(x, y)).flatten();
            let saved = match &granted {
                _ if virtual_output => virtual_key.clone(),
                Some(name) => super::permissions::monitor_key(name),
                None => key.to_owned(),
            };
            if cache.rotation.finish(&saved, fresh || virtual_output || granted.is_some()).is_err() {
                log::warn!("Portal permission cache could not be saved");
            }
            if wanted.is_some() && granted != wanted { log::warn!("Portal granted a different monitor than requested"); }
            // Keep a freshly granted compositor frame regardless of lock state. Discarding it
            // before new damage can leave a static desktop permanently blank (#2).
            let handle = Arc::new(Handle::new(handle));
            if !virtual_output {
                let clipboard_supported = unsafe { uu_portal_clipboard_supported(handle.0.as_ptr()) != 0 };
                if let Some(dir) = directory.as_deref() {
                    if super::permissions::save_clipboard_granted(dir, key, clipboard_supported).is_err() {
                        log::warn!("Portal clipboard capability cache could not be saved");
                    }
                }
            }
            // The engine records what it actually requested; a backend can refuse the mode the
            // environment or a runtime switch asked for.
            let available = unsafe { uu_portal_cursor_modes(handle.0.as_ptr()) };
            let applied = unsafe { uu_portal_cursor_mode(handle.0.as_ptr()) };
            if applied > 0 { super::CURSOR_MODE.store(if applied == 2 { 2 } else { 1 }, std::sync::atomic::Ordering::Release); }
            if available > 0 { super::CURSOR_EMBED.store(if available & 2 != 0 { 2 } else { 1 }, std::sync::atomic::Ordering::Release); }
            let initial_shm_handle = if !allow_dmabuf { handle.1 } else { 0 };
            Ok(Self { handle: Mutex::new(handle), locked: crate::session_lock::watch_locked(), was_locked: std::sync::atomic::AtomicBool::new(false), recovery: Mutex::new(Recovery::default()), size: pack(width, height).into(), monitor: Mutex::new(granted), virtual_output: virtual_output.into(), geometry_pending: std::sync::atomic::AtomicBool::new(false), clipboard_write: Mutex::new(()), end_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)), ended: std::sync::atomic::AtomicBool::new(false), dma_failed: std::sync::atomic::AtomicBool::new(false), dma_shm_only: std::sync::atomic::AtomicBool::new(!allow_dmabuf), dma_shm_handle: std::sync::atomic::AtomicU64::new(initial_shm_handle), installed: std::sync::atomic::AtomicBool::new(false) })
        }
        /// Frame pixels of the monitor the stream currently follows.
        pub fn size(&self) -> (u32, u32) {
            let packed = self.size.load(std::sync::atomic::Ordering::Acquire);
            ((packed >> 32) as u32, packed as u32)
        }
        /// A desktop controller is active again: allow stream-loss recovery after a prior `end()`
        /// and re-evaluate per-connection SHM fallback (keeping sticky `dma_failed` intact).
        /// Synchronized under `self.handle` so `begin()`, `end()`, and handle swaps share one boundary.
        pub fn begin(&self) {
            let _handle = self.handle.lock();
            self.ended.store(false, std::sync::atomic::Ordering::Release);
            self.dma_shm_only.store(false, std::sync::atomic::Ordering::Release);
        }
        /// Close the active compositor share without marking the desktop idle-ended (used by
        /// `set_virtual(false)` before immediately opening the physical monitor replacement).
        fn close_current_share(&self) {
            let active = self.handle.lock().ok().map(|handle| {
                self.dma_shm_handle.store(0, std::sync::atomic::Ordering::Release);
                Arc::clone(&*handle)
            });
            if let Some(handle) = active {
                #[cfg(test)]
                if handle.0 != NonNull::dangling() { unsafe { uu_portal_end(handle.0.as_ptr()) }; }
                #[cfg(not(test))]
                unsafe { uu_portal_end(handle.0.as_ptr()) };
            }
            let _serial = crate::input::lock_portal_held();
            crate::input::portal_session_replaced();
        }
        /// End the compositor session (screen-share indicator off); the next desktop
        /// connection reopens it with the saved consent.
        pub fn end(&self) {
            // Update `ended` and `end_generation` under `self.handle`, the same mutex that
            // recovery/installation holds when checking and swapping handles, then clone the
            // active handle out before the synchronous D-Bus Close call.
            let active = match self.handle.lock() {
                Ok(handle) => {
                    self.ended.store(true, std::sync::atomic::Ordering::Release);
                    self.dma_shm_handle.store(0, std::sync::atomic::Ordering::Release);
                    self.end_generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    Some(Arc::clone(&*handle))
                }
                Err(_) => {
                    self.ended.store(true, std::sync::atomic::Ordering::Release);
                    self.dma_shm_handle.store(0, std::sync::atomic::Ordering::Release);
                    self.end_generation.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    None
                }
            };
            if let Some(handle) = active {
                #[cfg(test)]
                if handle.0 != NonNull::dangling() { unsafe { uu_portal_end(handle.0.as_ptr()) }; }
                #[cfg(not(test))]
                unsafe { uu_portal_end(handle.0.as_ptr()) };
            }
            // The ended session's native devices are gone; forget the inputs bound to them.
            let _serial = crate::input::lock_portal_held();
            crate::input::portal_session_replaced();
        }
        #[cfg(test)]
        pub(crate) fn test_dead() -> Self {
            Self::test_dead_with_size(1920, 1080)
        }
        #[cfg(test)]
        pub(crate) fn test_dead_with_size(width: u32, height: u32) -> Self {
            let (_, locked) = tokio::sync::watch::channel(Some(false));
            Self {
                handle: Mutex::new(Arc::new(Handle::new(NonNull::dangling()))),
                locked,
                was_locked: std::sync::atomic::AtomicBool::new(false),
                recovery: Mutex::new(Recovery::default()),
                size: pack(width, height).into(),
                monitor: Mutex::new(None),
                virtual_output: false.into(),
                geometry_pending: std::sync::atomic::AtomicBool::new(false),
                clipboard_write: Mutex::new(()),
                end_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                ended: std::sync::atomic::AtomicBool::new(false),
                dma_failed: std::sync::atomic::AtomicBool::new(false),
                dma_shm_only: std::sync::atomic::AtomicBool::new(false),
                dma_shm_handle: std::sync::atomic::AtomicU64::new(0),
                installed: std::sync::atomic::AtomicBool::new(false),
            }
        }
        #[cfg(test)]
        pub(crate) fn test_set_size(&self, width: u32, height: u32) {
            self.size.store(pack(width, height), std::sync::atomic::Ordering::Release);
        }
        #[cfg(test)]
        pub(crate) fn test_lock_recovery(&self) -> impl Drop + '_ {
            self.recovery.lock().unwrap()
        }
        #[cfg(test)]
        pub(crate) fn test_ended(&self) -> bool {
            self.ended.load(std::sync::atomic::Ordering::Acquire)
        }
        /// Mutter connector of the monitor this session captures, when several are advertised.
        pub fn monitor(&self) -> Option<String> { self.monitor.lock().ok().and_then(|m| m.clone()) }
        /// Capture another monitor: open its own session (silently with saved consent; the first
        /// time GNOME asks on the host) and swap it in, so every holder of this Arc follows.
        pub fn switch_to(&self, connector: &str) -> Result<(), String> {
            use std::sync::atomic::Ordering;
            if self.monitor().as_deref() == Some(connector) { return Ok(()); }
            let generation = self.end_generation.load(Ordering::Acquire);
            if self.ended.load(Ordering::Acquire) { return Err("desktop session ended".into()); }
            // Both this and the cursor-mode rebuild replace the handle: serialize them so the
            // later open cannot install a stale source over the other.
            let _recovery = self.recovery.lock().map_err(|_| "Portal recovery lock poisoned")?;
            let is_cancelled = || self.ended.load(Ordering::Acquire)
                || self.end_generation.load(Ordering::Acquire) != generation;
            if is_cancelled() { return Err("desktop session ended".into()); }
            let allow_dmabuf = self.allow_dmabuf();
            let replacement = Self::open_inner(Some(&is_cancelled), Some(connector), false, false, allow_dmabuf).map_err(|error| error.to_string())?;
            if replacement.monitor().as_deref() != Some(connector) {
                return Err("Another monitor was shared; choose this monitor in the host dialog".into());
            }
            self.install_replacement(replacement, generation, false, false)?;
            if let Ok(mut monitor) = self.monitor.lock() { *monitor = Some(connector.to_owned()); }
            Ok(())
        }
        pub fn is_virtual(&self) -> bool { self.virtual_output.load(std::sync::atomic::Ordering::Acquire) }
        /// Privacy screen: capture and input move to a KWin virtual output (on) or back to the
        /// physical monitor (off). The caller enables physical outputs before switching back.
        pub fn set_virtual(&self, on: bool) -> Result<(), String> {
            use std::sync::atomic::Ordering;
            if self.is_virtual() == on { return Ok(()); }
            let generation = self.end_generation.load(Ordering::Acquire);
            // Serialize with `recover()` and `capture_changed()` before inspecting or updating
            // `virtual_output`, so a fast reconnect cannot race past an idle privacy exit and
            // reinstall a virtual share.
            let _recovery = self.recovery.lock().map_err(|_| "Portal recovery lock poisoned")?;
            if self.is_virtual() == on { return Ok(()); }
            let is_cancelled = || self.ended.load(Ordering::Acquire)
                || self.end_generation.load(Ordering::Acquire) != generation;
            let defer_physical = || {
                // Leaving privacy screen after the last controller disconnected (`end_idle`
                // already closed the share): switch the cached source back to physical and arm
                // `geometry_pending` so the next reconnect accepts the physical display's size.
                self.close_current_share();
                let _handle = self.handle.lock();
                self.virtual_output.store(false, Ordering::Release);
                self.geometry_pending.store(true, Ordering::Release);
                log::info!("Portal capture source: virtual=false (deferred while idle)");
            };
            if is_cancelled() {
                if !on {
                    defer_physical();
                    return Ok(());
                }
                return Err("desktop session ended".into());
            }
            let monitor = self.monitor();
            // KDE restores a monitor share against the whole workspace while the virtual output
            // exists (seen live: 3968x1280 = panel + virtual), so end the virtual share first.
            if !on { self.close_current_share(); }
            let allow_dmabuf = self.allow_dmabuf();
            let replacement = match Self::open_inner(Some(&is_cancelled), monitor.as_deref(), false, on, allow_dmabuf) {
                Ok(value) => value,
                Err(_) if !on && is_cancelled() => {
                    defer_physical();
                    return Ok(());
                }
                Err(error) => return Err(error.to_string()),
            };
            let size = match self.install_replacement(replacement, generation, false, false) {
                Ok(size) => size,
                Err(_) if !on && is_cancelled() => {
                    defer_physical();
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            self.virtual_output.store(on, Ordering::Release);
            log::info!("Portal capture source: virtual={on} size={}x{}", size.0, size.1);
            Ok(())
        }
        /// Ask the compositor to draw the pointer into the stream (embedded) or keep it out
        /// (hidden). The portal only reads this when the session is created, so swap in a fresh
        /// session with the saved consent; every holder of this Arc follows the new handle.
        /// Ok(true): the mode changed. Ok(false): nothing to do, or the desktop is locked and the
        /// next unlocked motion retries.
        pub fn set_cursor_embedded(&self, embedded: bool) -> Result<bool, String> {
            use std::sync::atomic::{AtomicBool, Ordering};
            if super::cursor_embedded() == embedded { return Ok(false); }
            // Sample before waiting for `recovery`: an end that happens while this worker is
            // queued must invalidate the rebuild below.
            let generation = self.end_generation.load(Ordering::Acquire);
            if self.ended.load(Ordering::Acquire) || *self.locked.borrow() != Some(false) { return Ok(false); }
            // A backend without the embedded cursor mode never applies the request; remember it
            // so motion does not spawn a rebuild every few events.
            let modes = self.handle.lock().map(|handle| unsafe { uu_portal_cursor_modes(handle.0.as_ptr()) }).unwrap_or(0);
            if modes > 0 && modes & 2 == 0 { super::CURSOR_EMBED.store(1, Ordering::Release); return Ok(false); }
            if modes & 2 != 0 { super::CURSOR_EMBED.store(2, Ordering::Release); }
            let _recovery = self.recovery.lock().map_err(|_| "Portal recovery lock poisoned")?;
            if super::cursor_embedded() == embedded { return Ok(false); }
            if *self.locked.borrow() != Some(false) { return Ok(false); }
            // The session this rebuild replaces must still be running: a viewer that left or an
            // idle shutdown ends it, and a replacement opened afterwards would share with nobody.
            if !self.is_alive() { return Ok(false); }
            if crate::input::portal_input_held() { return Ok(false); }
            let previous = super::cursor_embedded();
            let virtual_output = self.is_virtual();
            let monitor = self.monitor();
            // A lock or desktop end during the open must cancel the consent request, not wait
            // out its timeout.
            let cancel = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let watcher = {
                let locked = self.locked.clone();
                let end_generation = self.end_generation.clone();
                let cancel = cancel.clone();
                let stop = stop.clone();
                std::thread::Builder::new().name("portal-cursor-lock".into()).spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        if *locked.borrow() != Some(false) || end_generation.load(Ordering::Acquire) != generation {
                            cancel.store(true, Ordering::Release);
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }).ok()
            };
            unsafe { uu_portal_set_cursor_mode(if embedded { 2 } else { 1 }); }
            let result = (|| -> Result<Option<((u32, u32), i32)>, String> {
                let is_cancelled = || cancel.load(Ordering::Acquire)
                    || self.ended.load(Ordering::Acquire)
                    || self.end_generation.load(Ordering::Acquire) != generation;
                let allow_dmabuf = self.allow_dmabuf();
                let replacement = Self::open_inner(Some(&is_cancelled), monitor.as_deref(), false, virtual_output, allow_dmabuf)
                    .map_err(|error| error.to_string())?;
                if *self.locked.borrow() != Some(false) { return Err("desktop locked during the rebuild".into()); }
                if self.ended.load(Ordering::Acquire) || self.end_generation.load(Ordering::Acquire) != generation {
                    return Err("desktop session ended during the rebuild".into());
                }
                // A source change that finished meanwhile must not be undone by this swap.
                if self.is_virtual() != virtual_output || (!virtual_output && self.monitor() != monitor) {
                    return Err("capture source changed during the rebuild".into());
                }
                // A restored consent must not silently move the share to another monitor.
                if !virtual_output && monitor.is_some() && replacement.monitor() != monitor {
                    return Err("another monitor was shared; choose this monitor in the host dialog".into());
                }
                // KDE does not report the connector, so a size change is the signal that the
                // replacement follows another output.
                self.check_geometry(&replacement).map_err(|error| error.to_string())?;
                // A key or button can go down while the open waits for consent.
                if crate::input::portal_input_held() { return Ok(None); }
                let size = match self.install_replacement(replacement, generation, true, virtual_output) {
                    Ok(size) => size,
                    Err(error) if error == "input held during the swap" => return Ok(None),
                    Err(error) => return Err(error),
                };
                let applied = self.handle.lock().ok().map(|handle| unsafe { uu_portal_cursor_mode(handle.0.as_ptr()) }).unwrap_or(0);
                Ok(Some((size, applied)))
            })();
            stop.store(true, Ordering::Release);
            drop(watcher);
            match result {
                Ok(Some(((width, height), applied))) => {
                    let applied_embedded = applied == 2;
                    super::CURSOR_MODE.store(if applied_embedded { 2 } else { 1 }, Ordering::Release);
                    if applied_embedded != embedded {
                        // The backend refused the mode (or cannot report support); retrying would
                        // rebuild the session every motion run without changing anything.
                        super::CURSOR_EMBED.store(1, Ordering::Release);
                        unsafe { uu_portal_set_cursor_mode(applied) };
                        log::info!("Portal backend did not apply the embedded cursor");
                        return Ok(false);
                    }
                    log::info!("Portal cursor mode: embedded={embedded} size={width}x{height}");
                    Ok(true)
                }
                Ok(None) => {
                    // Deferred (input went down during the open): open_inner published the
                    // replacement's mode, but that session is never installed.
                    self.restore_installed_cursor_mode(previous);
                    Ok(false)
                }
                Err(error) => {
                    // Leave the engine on the mode the installed session actually has.
                    self.restore_installed_cursor_mode(previous);
                    Err(error)
                }
            }
        }
        pub fn open() -> Result<Self, String> {
            let session = Self::open_inner(None, None, false, false, true).map_err(|error| error.to_string())?;
            session.install_capture_proof();
            Ok(session)
        }
        #[cfg(test)]
        pub(crate) fn test_lock_watch(&mut self, locked: tokio::sync::watch::Receiver<Option<bool>>) {
            self.locked = locked;
        }
        /// A consent saved before touch support restores keyboard and pointer only. Ask the host once
        /// more (GNOME shows its share dialog; the consent timeout closes it) and swap the new session
        /// in when it grants the touchscreen for this monitor at this size; until then, and on any
        /// refusal, this session and its saved consent stay as they are. Ok(false): nothing to do.
        pub fn upgrade_touch(&self) -> Result<bool, String> {
            use std::sync::atomic::Ordering;
            let available = self.handle.lock().is_ok_and(|handle| unsafe { uu_portal_touch_available(handle.0.as_ptr()) } != 0);
            if !available || self.touch_supported() { return Ok(false); }
            let generation = self.end_generation.load(Ordering::Acquire);
            if self.ended.load(Ordering::Acquire) { return Ok(false); }
            // Serialize with recovery and the cursor-mode rebuild: all replace the handle.
            let _recovery = self.recovery.lock().map_err(|_| "Portal recovery lock poisoned")?;
            let is_cancelled = || self.ended.load(Ordering::Acquire)
                || self.end_generation.load(Ordering::Acquire) != generation;
            if is_cancelled() { return Ok(false); }
            log::info!("Portal touch consent: asking the host again for the touchscreen");
            let monitor = self.monitor();
            let allow_dmabuf = self.allow_dmabuf();
            let replacement = Self::open_inner(Some(&is_cancelled), monitor.as_deref(), true, self.is_virtual(), allow_dmabuf).map_err(|error| error.to_string())?;
            // The new consent is saved for whichever monitor was granted, so a later connection uses it.
            if replacement.monitor() != monitor { return Err("another monitor was shared".into()); }
            if !replacement.touch_supported() { return Err("the touchscreen was not granted".into()); }
            self.check_geometry(&replacement).map_err(|error| error.to_string())?;
            self.install_replacement(replacement, generation, false, self.is_virtual())?;
            Ok(true)
        }
        pub fn open_cancellable(cancelled: &std::sync::atomic::AtomicBool) -> Result<Self, super::PortalError> {
            let is_cancelled = || cancelled.load(std::sync::atomic::Ordering::Acquire);
            let session = Self::open_inner(Some(&is_cancelled), None, false, false, true)?;
            session.install_capture_proof();
            Ok(session)
        }
        pub fn recover(&self, cancelled: &std::sync::atomic::AtomicBool) -> Result<bool, super::PortalError> {
            use std::sync::atomic::Ordering;
            // Snapshot `end_generation` BEFORE waiting on `self.recovery`: a queued recover task
            // must see any `end()` / `end_idle()` that happened before or while waiting for the
            // lock, and a session ended by `end()` stays closed until `begin()` explicitly re-arms it.
            let generation = self.end_generation.load(Ordering::Acquire);
            let is_cancelled = || cancelled.load(Ordering::Acquire)
                || self.ended.load(Ordering::Acquire)
                || self.end_generation.load(Ordering::Acquire) != generation;
            if is_cancelled() { return Err(super::PortalError { code: 13 }); }
            // Serialize with the encoder's stream-loss recovery: two openers would ask for consent twice.
            // The wait is bounded: another opener (encoder reopen, consent rebuild) can hold this lock
            // for as long as a consent dialog waits, and an unbounded spin here would leave the desktop
            // supervisor's attempt silently running with no log and no frames until the viewer times out.
            let lock_wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let mut recovery = loop {
                match self.recovery.try_lock() {
                    Ok(recovery) => break recovery,
                    Err(TryLockError::WouldBlock) => {
                        if is_cancelled() { return Err(super::PortalError { code: 13 }); }
                        if std::time::Instant::now() >= lock_wait_deadline {
                            log::warn!("Portal recovery lock held by another opener for 15s; retrying later");
                            return Err(super::PortalError { code: 3 });
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(TryLockError::Poisoned(_)) => return Err(super::PortalError { code: 3 }),
                }
            };
            if is_cancelled() { return Err(super::PortalError { code: 13 }); }
            if recovery.failed_until.is_some_and(|until| std::time::Instant::now() < until) {
                return Err(super::PortalError { code: 3 });
            }
            let alive = {
                let handle = self.handle.lock().map_err(|_| super::PortalError { code: 3 })?;
                unsafe { uu_portal_alive(handle.0.as_ptr()) != 0 && uu_portal_capture_alive(handle.0.as_ptr()) != 0 }
            };
            if alive { return Ok(false); }

            // The privacy screen's virtual share is reopened as itself, not as the monitor.
            let virtual_output = self.is_virtual();
            let monitor = self.monitor();
            let allow_dmabuf = self.allow_dmabuf();
            let result = super::open_then_install_when(&is_cancelled, || Self::open_inner(Some(&is_cancelled), monitor.as_deref(), false, virtual_output, allow_dmabuf), |replacement| {
                let updated_size = self.check_recovery_geometry(&replacement)?;
                let replacement_handle = replacement.handle.into_inner().map_err(|_| super::PortalError { code: 3 })?;
                let clipboard = self.clipboard_write.lock().map_err(|_| super::PortalError { code: 3 })?;
                let serial = crate::input::lock_portal_held();
                let mut handle = self.handle.lock().map_err(|_| super::PortalError { code: 3 })?;
                if is_cancelled() {
                    drop(handle);
                    drop(serial);
                    drop(clipboard);
                    drop(replacement_handle);
                    return Err(super::PortalError { code: 13 });
                }
                if let Some((width, height)) = updated_size {
                    self.size.store(pack(width, height), Ordering::Release);
                    self.geometry_pending.store(false, Ordering::Release);
                }
                let previous_handle = std::mem::replace(&mut *handle, replacement_handle);
                let installed = Arc::clone(&*handle);
                let shm_handle = if self.dma_shm_only.load(Ordering::Acquire) {
                    installed.1
                } else { 0 };
                self.dma_shm_handle.store(shm_handle, Ordering::Release);
                self.install_capture_proof();
                drop(handle);
                if !self.allow_dmabuf() {
                    unsafe { uu_portal_dmabuf_disable(installed.0.as_ptr()) };
                }
                unsafe { uu_portal_clipboard_copy(previous_handle.0.as_ptr(), installed.0.as_ptr()) };
                drop(clipboard);
                // The replaced devices took any held portal input with them.
                crate::input::portal_session_replaced();
                drop(serial);
                drop(previous_handle);
                clear_pause_for_unlocked_replacement(&self.was_locked, *self.locked.borrow());
                log::info!("Portal capture session restored after closure");
                Ok(())
            });
            match result {
                Ok(()) => { recovery.succeeded(); Ok(true) }
                Err(error) => {
                    if error.code != 13 { recovery.failed(std::time::Instant::now(), error.code); }
                    Err(error)
                }
            }
        }
        pub fn pause(&self) {
            if let Ok(handle) = self.handle.lock() {
                if unsafe { uu_portal_capture_alive(handle.0.as_ptr()) } != 0 { return; }
                unsafe { uu_portal_reset_frame(handle.0.as_ptr()) };
                self.was_locked.store(true, std::sync::atomic::Ordering::Release);
            } else {
                self.was_locked.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        pub fn capture(&self, frame: &mut [u8]) -> Result<(), String> {
            let mut version = None;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            loop {
                self.capture_changed(frame, &mut version)?;
                // A one-shot snapshot must wait for SHM renegotiation after DMA initialization.
                // Release the handle between attempts so input and recovery remain responsive.
                if version != Some(super::FrameVersion(0, 0)) || std::time::Instant::now() >= deadline { return Ok(()); }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        /// Sticky session-level DMA-BUF disable: called when DMA-BUF is unsupported, failed or
        /// abandoned on this host; survives across desktop reconnects.
        pub fn disable_dmabuf(&self) {
            let _ = self.disable_dmabuf_for(None);
        }
        /// Sticky DMA-BUF disable scoped to `expected_handle` (`None` = host/driver-wide). If a
        /// replacement handle was already installed after `expected_handle`, returns `false`
        /// without poisoning `dma_failed` or disabling the newer handle.
        pub fn disable_dmabuf_for(&self, expected_handle: Option<u64>) -> bool {
            let Ok(handle) = self.handle.lock() else { return false; };
            if expected_handle.is_some_and(|expected| handle.1 != expected) {
                return false;
            }
            self.dma_failed.store(true, std::sync::atomic::Ordering::Release);
            #[cfg(test)]
            if handle.0 == NonNull::dangling() { return true; }
            unsafe { uu_portal_dmabuf_disable(handle.0.as_ptr()) };
            true
        }
        /// Per-connection SHM capture preference (CPU or FFmpeg process pipeline): disables
        /// DMA-BUF for the current connection and its recoveries, while `begin()` re-evaluates
        /// DMA-BUF on a new desktop connection if DMA-BUF itself never failed.
        pub fn use_shm_capture(&self) {
            self.dma_shm_only.store(true, std::sync::atomic::Ordering::Release);
            if let Ok(handle) = self.handle.lock() {
                self.dma_shm_handle.store(handle.1, std::sync::atomic::Ordering::Release);
                #[cfg(test)]
                if handle.0 == NonNull::dangling() { return; }
                unsafe { uu_portal_dmabuf_disable(handle.0.as_ptr()) };
            }
        }
        /// Whether the loaded engine exports the level-triggered wait (engines from 0.2.66 on).
        /// Resolved once: `dlsym` on every frame wait would also warn on every frame for an old engine.
        fn level_triggered_wait() -> bool {
            static READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *READY.get_or_init(|| {
                #[cfg(all(target_os = "linux", feature = "engine-plugins"))]
                { crate::engine_plugin::symbol(crate::engine_plugin::Engine::Portal, "uu_portal_wait_frame_since\0") != 0 }
                #[cfg(not(all(target_os = "linux", feature = "engine-plugins")))]
                { true }
            })
        }
        /// Whether the loaded engine exports `uu_portal_dmabuf_abandon`.
        fn dmabuf_abandon_supported() -> bool {
            static READY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *READY.get_or_init(|| {
                #[cfg(all(target_os = "linux", feature = "engine-plugins"))]
                { crate::engine_plugin::symbol(crate::engine_plugin::Engine::Portal, "uu_portal_dmabuf_abandon\0") != 0 }
                #[cfg(not(all(target_os = "linux", feature = "engine-plugins")))]
                { true }
            })
        }
        pub fn with_dmabuf<T>(&self, version: &mut Option<super::FrameVersion>,
            encode: impl FnOnce(&super::DmaFrame, bool) -> Result<T, String>,
        ) -> Result<super::DmaCapture<T>, String> {
            self.with_dmabuf_controlled(None, version, |frame, changed, _handle_id, _abandon| encode(frame, changed))
        }
        /// Like `with_dmabuf`, but verifies `expected_size` before invoking `encode`, passes the
        /// leased handle id, and gives the caller an `abandon` flag: setting it to `true` instructs
        /// the lease guard not to queue the current DMA-BUF back to the producer on drop.
        pub fn with_dmabuf_controlled<T>(&self, expected_size: Option<(u32, u32)>,
            version: &mut Option<super::FrameVersion>,
            encode: impl FnOnce(&super::DmaFrame, bool, u64, &mut bool) -> Result<T, String>,
        ) -> Result<super::DmaCapture<T>, String> {
            self.with_dmabuf_with_acquire(
                expected_size,
                version,
                |handle, frame, revision| {
                    #[cfg(test)]
                    if handle.0 == NonNull::dangling() { return 10; }
                    unsafe { uu_portal_dmabuf_acquire(handle.0.as_ptr(), frame, revision) }
                },
                encode,
            )
        }
        fn with_dmabuf_with_acquire<T>(&self, expected_size: Option<(u32, u32)>,
            version: &mut Option<super::FrameVersion>,
            acquire: impl FnOnce(&Arc<Handle>, &mut super::DmaFrame, &mut u64) -> i32,
            encode: impl FnOnce(&super::DmaFrame, bool, u64, &mut bool) -> Result<T, String>,
        ) -> Result<super::DmaCapture<T>, String> {
            use std::sync::atomic::Ordering;
            // A sticky DMA-BUF failure reports Unavailable(14, None), while a per-connection SHM
            // preference (or a still-live SHM handle carried across a fast takeover) routes to
            // host upload (`Cpu`) without tripping sticky `disable_dmabuf()`. Require
            // `uu_portal_dmabuf_abandon` before taking a DMA-BUF lease so an older plugin cannot
            // re-queue a wedged GPU buffer to the producer.
            if self.dma_failed.load(Ordering::Acquire) {
                return Ok(super::DmaCapture::Unavailable(14, None));
            }
            if self.dma_shm_only.load(Ordering::Acquire) {
                return Ok(super::DmaCapture::Cpu);
            }
            // A handle is reference counted out of the mutex, so a monitor swap keeps the session (and
            // with it the lease) alive while the GPU copies: the mutex stays free for input injection
            // and clipboard, which otherwise waited a whole frame's GPU fence at 120 fps.
            let (handle, generation) = {
                let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
                if expected_size.is_some_and(|want| self.size() != want) {
                    return Ok(super::DmaCapture::SizeChanged);
                }
                let generation = self.end_generation.load(Ordering::Acquire);
                if self.ended.load(Ordering::Acquire) {
                    return Ok(super::DmaCapture::Dead);
                }
                if self.dma_shm_handle.load(Ordering::Acquire) == handle.1 {
                    return Ok(super::DmaCapture::Cpu);
                }
                if !Self::dmabuf_abandon_supported() {
                    return Ok(super::DmaCapture::Unavailable(14, None));
                }
                #[cfg(test)]
                let skip_ffi = handle.0 == NonNull::dangling();
                #[cfg(not(test))]
                let skip_ffi = false;
                if !skip_ffi {
                    unsafe { uu_portal_input_dispatch(handle.0.as_ptr()) };
                    if unsafe { uu_portal_capture_alive(handle.0.as_ptr()) } == 0 {
                        return Ok(super::DmaCapture::Dead);
                    }
                }
                (Arc::clone(&*handle), generation)
            };
            let handle_id = handle.1;
            let version_key = handle_id as usize;
            let mut revision = version.filter(|v| v.0 == version_key).map_or(0, |v| v.1);
            let mut frame = super::DmaFrame::default();
            match acquire(&handle, &mut frame, &mut revision) {
                code @ (0 | 12) => {
                    struct Lease { ptr: NonNull<c_void>, abandon: bool }
                    impl Drop for Lease {
                        fn drop(&mut self) {
                            #[cfg(test)]
                            if self.ptr == NonNull::dangling() { return; }
                            if self.abandon {
                                unsafe { uu_portal_dmabuf_abandon(self.ptr.as_ptr()) };
                            } else {
                                unsafe { uu_portal_dmabuf_release(self.ptr.as_ptr()) };
                            }
                        }
                    }
                    let mut abandon = false;
                    let encoded = {
                        let mut lease = Lease { ptr: handle.0, abandon: false };
                        if expected_size.is_some_and(|want| (frame.width, frame.height) != want) {
                            drop(lease);
                            *version = None;
                            return Ok(super::DmaCapture::SizeChanged);
                        }
                        let out = encode(&frame, code == 0, handle_id, &mut abandon);
                        if abandon {
                            self.dma_failed.store(true, Ordering::Release);
                            lease.abandon = true;
                        }
                        out?
                    };
                    if abandon {
                        *version = None;
                        return Ok(super::DmaCapture::Frame(encoded));
                    }
                    // The lease (and the PipeWire lock it pins) is gone before the mutex is taken
                    // again: input dispatch holds that mutex and wants this same PipeWire lock.
                    let current = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
                    if Arc::ptr_eq(&handle, &*current) {
                        if code == 0 { self.note_frame(); }
                        *version = Some(super::FrameVersion(version_key, revision));
                    } else {
                        // A monitor swap installed another session while the GPU held this lease. The
                        // AU belongs to the old source and is still sent - the encoder has already
                        // advanced its reconstruction, so dropping it would leave the decoder one
                        // reference behind - but it is no proof for the new session and its frame
                        // version no longer applies.
                        *version = None;
                    }
                    Ok(super::DmaCapture::Frame(encoded))
                },
                10 => Ok(super::DmaCapture::Pending),
                15 => Ok(super::DmaCapture::Blank),
                code => {
                    // Re-check session state under `self.handle` so a concurrent `end()`, handle
                    // swap, compositor closure (`code == 9` / `!capture_alive`), or `use_shm_capture()`
                    // between lock release and `uu_portal_dmabuf_acquire` never masquerades as a
                    // permanent DMA-BUF failure (`Unavailable`) or disables a newer handle.
                    let current = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
                    if !Arc::ptr_eq(&handle, &*current) {
                        *version = None;
                        return Ok(super::DmaCapture::Pending);
                    }
                    if expected_size.is_some_and(|want| self.size() != want) {
                        *version = None;
                        return Ok(super::DmaCapture::SizeChanged);
                    }
                    if self.dma_failed.load(Ordering::Acquire) {
                        return Ok(super::DmaCapture::Unavailable(code, None));
                    }
                    if self.dma_shm_only.load(Ordering::Acquire)
                        || self.dma_shm_handle.load(Ordering::Acquire) == handle_id
                    {
                        return Ok(super::DmaCapture::Cpu);
                    }
                    if self.ended.load(Ordering::Acquire)
                        || self.end_generation.load(Ordering::Acquire) != generation
                        || code == 9
                    {
                        return Ok(super::DmaCapture::Dead);
                    }
                    #[cfg(test)]
                    let alive = handle.0 == NonNull::dangling() || unsafe { uu_portal_capture_alive(handle.0.as_ptr()) } != 0;
                    #[cfg(not(test))]
                    let alive = unsafe { uu_portal_capture_alive(handle.0.as_ptr()) } != 0;
                    if !alive {
                        return Ok(super::DmaCapture::Dead);
                    }
                    Ok(super::DmaCapture::Unavailable(code, Some(handle_id)))
                }
            }
        }
        pub fn capture_changed(&self, frame: &mut [u8], version: &mut Option<super::FrameVersion>) -> Result<bool, String> {
            self.capture_changed_inner(frame, version, true, |handle| {
                unsafe { uu_portal_input_dispatch(handle.0.as_ptr()) };
                unsafe { uu_portal_capture_alive(handle.0.as_ptr()) != 0 }
            }).map(|changed| changed.unwrap_or(false))
        }
        /// Handle a dead/ended stream returned by `with_dmabuf_controlled` without downgrading a
        /// newly installed live DMA-BUF handle to SHM. Returns `Ok(None)` if a live DMA-BUF-eligible
        /// handle is now installed so the caller retries DMA-BUF instead of switching to SHM.
        pub fn capture_dead_or_retry(&self, frame: &mut [u8], version: &mut Option<super::FrameVersion>) -> Result<Option<bool>, String> {
            self.capture_changed_inner(frame, version, false, |handle| {
                unsafe { uu_portal_input_dispatch(handle.0.as_ptr()) };
                unsafe { uu_portal_capture_alive(handle.0.as_ptr()) != 0 }
            })
        }
        fn capture_changed_inner(&self, frame: &mut [u8], version: &mut Option<super::FrameVersion>,
            allow_shm_upgrade: bool, check_alive: impl FnOnce(&Handle) -> bool,
        ) -> Result<Option<bool>, String> {
            let (width, height) = self.size();
            // A buffer sized for the previous monitor belongs to an encoder that a switch is replacing.
            if frame.len() != width as usize * height as usize * 4 { return Ok(Some(blank_changed(frame, version))); }
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            if !check_alive(&handle) {
                // A healthy source is forwarded regardless of lock state. Only an unavailable
                // source is blanked; wait for unlock before reopening a compositor-closed session.
                if *self.locked.borrow() != Some(false) {
                    unsafe { uu_portal_reset_frame(handle.0.as_ptr()) };
                    self.was_locked.store(true, std::sync::atomic::Ordering::Release);
                    return Ok(Some(blank_changed(frame, version)));
                }
                if self.was_locked.load(std::sync::atomic::Ordering::Acquire) {
                    if self.take_lock_observation()? { unsafe { uu_portal_reset_frame(handle.0.as_ptr()) }; }
                    return Ok(Some(blank_changed(frame, version)));
                }
                if self.ended.load(std::sync::atomic::Ordering::Acquire) {
                    return Ok(Some(blank_changed(frame, version)));
                }
                // Bind this recovery attempt to the session's current end_generation BEFORE taking
                // the recovery lock so a desktop end cancels the open and rejects a late swap.
                let generation = self.end_generation.load(std::sync::atomic::Ordering::Acquire);
                // Serialize recovery without making another capture worker hold
                // the input mutex while waiting for this worker's Portal calls.
                let mut recovery = match self.recovery.try_lock() {
                    Ok(retry) => retry,
                    Err(TryLockError::WouldBlock) => return Ok(Some(blank_changed(frame, version))),
                    Err(TryLockError::Poisoned(_)) => return Err("Portal recovery lock poisoned".into()),
                };
                let is_cancelled = || self.end_generation.load(std::sync::atomic::Ordering::Acquire) != generation
                    || self.ended.load(std::sync::atomic::Ordering::Acquire)
                    || *self.locked.borrow() != Some(false)
                    || self.was_locked.load(std::sync::atomic::Ordering::Acquire);
                let pending_size = std::cell::Cell::new(None::<(u32, u32)>);
                if replace_handle_carrying(&self.handle, &self.clipboard_write, handle, || {
                    {
                        let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
                        if unsafe { uu_portal_capture_alive(handle.0.as_ptr()) } != 0 {
                            recovery.retry_at = None;
                            return Ok(None);
                        }
                    }
                    if is_cancelled() { return Ok(None); }
                    // logind's LockedHint can lag compositor stream closure.
                    if !recovery.ready(std::time::Instant::now(), || {
                        if crate::session_lock::locked_now() == Some(false) { return true; }
                        self.was_locked.store(true, std::sync::atomic::Ordering::Release);
                        false
                    }) { return Ok(None); }
                    let allow_dmabuf = self.allow_dmabuf();
                    let replacement = match Self::open_inner(Some(&is_cancelled), self.monitor().as_deref(), false, self.is_virtual(), allow_dmabuf) {
                        Ok(value) => value,
                        Err(error) => {
                            if error.code != 13 {
                                log::warn!("Portal capture recovery deferred: {error}");
                                recovery.failed(std::time::Instant::now(), error.code);
                            }
                            return Ok(None);
                        }
                    };
                    recovery.succeeded();
                    pending_size.set(self.check_recovery_geometry(&replacement).map_err(|error| error.to_string())?);
                    // Consent can outlive the unlocked state or session ownership checked above.
                    if is_cancelled() { return Ok(None); }
                    replacement.handle.into_inner().map(Some).map_err(|_| "Portal session lock poisoned".into())
                }, || !is_cancelled(), || {
                    if let Some((width, height)) = pending_size.get() {
                        self.size.store(pack(width, height), std::sync::atomic::Ordering::Release);
                        self.geometry_pending.store(false, std::sync::atomic::Ordering::Release);
                    }
                    self.dma_shm_handle.store(0, std::sync::atomic::Ordering::Release);
                    self.install_capture_proof();
                })? {
                    if !self.allow_dmabuf() {
                        if let Ok(active) = self.handle.lock() {
                            if self.dma_shm_only.load(std::sync::atomic::Ordering::Acquire) {
                                self.dma_shm_handle.store(active.1, std::sync::atomic::Ordering::Release);
                            }
                            unsafe { uu_portal_dmabuf_disable(active.0.as_ptr()) };
                        }
                    }
                    *version = None;
                    log::info!("Portal capture session restored after stream loss");
                }
                // Recheck lock state on the next capture before showing the new session.
                return Ok(Some(blank_changed(frame, version)));
            } else {
                self.was_locked.store(false, std::sync::atomic::Ordering::Release);
                match self.recovery.try_lock() {
                    Ok(mut recovery) => recovery.retry_at = None,
                    Err(TryLockError::WouldBlock) => (),
                    Err(TryLockError::Poisoned(_)) => return Err("Portal recovery lock poisoned".into()),
                }
            }
            let handle_id = handle.1;
            if !allow_shm_upgrade && self.allow_dmabuf()
                && self.dma_shm_handle.load(std::sync::atomic::Ordering::Acquire) != handle_id
            {
                return Ok(None);
            }
            // Reading SHM frames calls uu_portal_dmabuf_disable on the C handle; mirror that onto
            // this connection's SHM preference (reset on next desktop connection's `begin()`).
            let version_key = handle_id as usize;
            self.dma_shm_only.store(true, std::sync::atomic::Ordering::Release);
            self.dma_shm_handle.store(handle_id, std::sync::atomic::Ordering::Release);
            let mut revision = version.filter(|v| v.0 == version_key).map_or(0, |v| v.1);
            let code = unsafe { uu_portal_capture_since(handle.0.as_ptr(), frame.as_mut_ptr(), frame.len(), &mut revision) };
            match code {
                0 | 12 => {
                    *version = Some(super::FrameVersion(version_key, revision));
                    if code == 0 { self.note_frame(); }
                    Ok(Some(code == 0))
                }
                9 | 10 => Ok(Some(blank_changed(frame, version))),
                _ => Err(super::error(code)),
            }
        }

        pub fn clipboard_supported(&self) -> bool {
            self.handle.lock().is_ok_and(|handle| {
                #[cfg(test)]
                if handle.0 == NonNull::dangling() { return false; }
                unsafe { uu_portal_clipboard_supported(handle.0.as_ptr()) != 0 }
            })
        }
        /// Waits until the compositor produced a frame newer than `seen` (which is advanced to the
        /// newest sequence) or `timeout` passes. Level triggered, so a frame that arrived while the
        /// previous one was still being encoded is not missed.
        pub fn wait_frame(&self, seen: &mut u64, timeout: std::time::Duration) -> Result<bool, String> {
            let handle = lease_handle(&self.handle)?;
            // Retain sub-millisecond precision across the FFI boundary and clamp to 5s.
            let us = (timeout.as_micros().min(5_000_000) as i64).max(1);
            let ret = if Self::level_triggered_wait() {
                let mut sequence = *seen;
                let ret = unsafe { uu_portal_wait_frame_since(handle.0.as_ptr(), &mut sequence, us) };
                *seen = sequence;
                ret
            } else {
                // An engine older than the seen-sequence API only reports a change since this call.
                unsafe { uu_portal_wait_frame(handle.0.as_ptr(), us) }
            };
            if ret < 0 {
                Err("Portal stream closed or failed during frame wait".into())
            } else {
                Ok(ret > 0)
            }
        }
        pub fn clipboard_ready(&self) -> bool {
            self.handle.lock().is_ok_and(|handle| {
                #[cfg(test)]
                if handle.0 == NonNull::dangling() { return false; }
                unsafe { uu_portal_clipboard_ready(handle.0.as_ptr()) != 0 }
            })
        }
        pub fn clipboard_set(&self, text: &str) -> Result<(), String> {
            if text.len()>crate::clipboard::MAX_TEXT || *self.locked.borrow()!=Some(false) { return Err("Portal clipboard unavailable".into()); }
            // Serialize with the cursor-session swap: it carries the old text over before
            // releasing this lock, so a update that arrives during the swap wins afterwards.
            let _write=self.clipboard_write.lock().map_err(|_| "Portal clipboard lock poisoned")?;
            let handle=self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            if unsafe { uu_portal_clipboard_set(handle.0.as_ptr(),text.as_ptr(),text.len()) }==0 {Ok(())} else {Err("Portal clipboard ownership failed".into())}
        }
        pub fn clipboard_poll(&self) -> Option<String> {
            if *self.locked.borrow()!=Some(false) { return None; }
            let handle=self.handle.lock().ok()?;
            #[cfg(test)]
            if handle.0 == NonNull::dangling() { return None; }
            let mut bytes=vec![0;crate::clipboard::MAX_TEXT];
            let length=unsafe { uu_portal_clipboard_poll(handle.0.as_ptr(),bytes.as_mut_ptr(),bytes.len()) };
            if length<0 { return None; }
            bytes.truncate(length as usize); String::from_utf8(bytes).ok()
        }
        pub fn move_pointer(&self, x: f64, y: f64, relative: bool) -> Result<(), String> {
            if *self.locked.borrow() != Some(false) { return Err("Portal input blocked while locked".into()); }
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            super::check(unsafe { uu_portal_move(handle.0.as_ptr(), x, y, i32::from(relative)) })
        }
        pub fn button(&self, button: i32, pressed: bool) -> Result<(), String> {
            if pressed && *self.locked.borrow() != Some(false) { return Err("Portal input blocked while locked".into()); }
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            super::check(unsafe { uu_portal_button(handle.0.as_ptr(), button, i32::from(pressed)) })
        }
        /// Press and release a keysym through Notify*: the compositor picks the key and level of
        /// the active layout. Under EIS (keycodes only) it fails before typing anything.
        pub fn type_keysym(&self, symbol: u32) -> Result<(), String> {
            if *self.locked.borrow() != Some(false) { return Err("Portal input blocked while locked".into()); }
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            super::check(unsafe { uu_portal_key(handle.0.as_ptr(), symbol, -1, 1) })?;
            super::check(unsafe { uu_portal_key(handle.0.as_ptr(), symbol, -1, 0) })
        }
        /// Smooth scroll in stream pixels (positive scrolls down/right); `finish` ends the gesture.
        pub fn scroll(&self, dx: f64, dy: f64, finish: bool) -> Result<(), String> {
            if *self.locked.borrow() != Some(false) { return Err("Portal input blocked while locked".into()); }
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            super::check(unsafe { uu_portal_scroll(handle.0.as_ptr(), dx, dy, i32::from(finish)) })
        }
        /// The portal granted the touchscreen device type (requested where available).
        pub fn touch_supported(&self) -> bool {
            self.handle.lock().is_ok_and(|handle| unsafe { uu_portal_touch_supported(handle.0.as_ptr()) } != 0)
        }
        /// One finger in stream pixels: phase 0 down, 1 motion, 2 up.
        pub fn touch(&self, phase: i32, x: f64, y: f64) -> Result<(), String> {
            if phase != 2 && *self.locked.borrow() != Some(false) { return Err("Portal input blocked while locked".into()); }
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            super::check(unsafe { uu_portal_touch(handle.0.as_ptr(), phase, x, y) })
        }
        pub fn key(&self, symbol: u64, pressed: bool) -> Result<(), String> {
            if pressed && *self.locked.borrow() != Some(false) { return Err("Portal input blocked while locked".into()); }
            let symbol = u32::try_from(symbol).map_err(|_| "Invalid Portal keysym")?;
            let handle = self.handle.lock().map_err(|_| "Portal session lock poisoned")?;
            let keycode = crate::uinput::keysym_to_evdev(u64::from(symbol)).map_or(-1, i32::from);
            super::check(unsafe { uu_portal_key(handle.0.as_ptr(), symbol, keycode, i32::from(pressed)) })
        }
    }

    /// Mutter's monitors (GNOME); empty elsewhere.
    pub fn monitors() -> Vec<super::Monitor> {
        if crate::engine_plugin::load(crate::engine_plugin::Engine::Portal).is_err() { return Vec::new(); }
        let mut text = vec![0u8; 8192];
        if unsafe { uu_portal_monitors(text.as_mut_ptr() as *mut libc::c_char, text.len()) } <= 0 { return Vec::new(); }
        let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
        super::parse_monitors(&String::from_utf8_lossy(&text[..end]))
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{mem::ManuallyDrop, sync::Arc};

        struct Fixture(ManuallyDrop<Session>);
        impl Fixture {
            fn new(physical: (u32, u32)) -> Self {
                let (_, locked) = tokio::sync::watch::channel(None);
                Self(ManuallyDrop::new(Session {
                    handle: Mutex::new(Arc::new(Handle::new(NonNull::dangling()))),
                    locked,
                    was_locked: std::sync::atomic::AtomicBool::new(false),
                    recovery: Mutex::new(Recovery::default()),
                    size: pack(physical.0, physical.1).into(),
                    monitor: Mutex::new(None), virtual_output: false.into(),
                    geometry_pending: std::sync::atomic::AtomicBool::new(false),
                    clipboard_write: Mutex::new(()),
                    end_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    ended: std::sync::atomic::AtomicBool::new(false),
                    dma_failed: std::sync::atomic::AtomicBool::new(false),
                    dma_shm_only: std::sync::atomic::AtomicBool::new(false),
                    dma_shm_handle: std::sync::atomic::AtomicU64::new(0),
                    installed: std::sync::atomic::AtomicBool::new(false),
                }))
            }

            fn session(&self) -> &Session { &self.0 }
        }
        impl Drop for Fixture {
            fn drop(&mut self) {
                let Session { handle, locked, was_locked, recovery, .. } =
                    unsafe { ManuallyDrop::take(&mut self.0) };
                let handle = handle.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
                std::mem::forget(handle);
                drop((locked, was_locked, recovery));
            }
        }

        #[test]
        fn recovery_requires_only_matching_frame_dimensions() {
            let original = Fixture::new((3840, 2160));
            let same = Fixture::new((3840, 2160));
            let width_changed = Fixture::new((2560, 2160));
            let height_changed = Fixture::new((3840, 1440));

            assert!(original.session().check_geometry(same.session()).is_ok());
            assert!(original.session().check_geometry(width_changed.session()).is_err());
            assert!(original.session().check_geometry(height_changed.session()).is_err());
        }

        #[test]
        fn a_candidate_publishes_privacy_readiness_only_after_installation() {
            let candidate = Fixture::new((1920, 1080));
            let frames = std::sync::atomic::AtomicU64::new(1);

            super::super::note_capture_frame(candidate.session().installed.load(std::sync::atomic::Ordering::Acquire), &frames);
            assert_eq!(frames.load(std::sync::atomic::Ordering::Acquire), 1, "a discarded candidate must leave the installed session's proof intact");

            candidate.session().install_capture_proof();
            assert!(candidate.session().installed.load(std::sync::atomic::Ordering::Acquire));
            super::super::note_capture_frame(candidate.session().installed.load(std::sync::atomic::Ordering::Acquire), &frames);
            assert_eq!(frames.load(std::sync::atomic::Ordering::Acquire), 2, "the installed capture must publish its first fresh frame");
        }

        /// upgrade_touch() holds the recovery mutex for as long as its consent dialog can wait
        /// (up to the consent timeout), and the desktop supervisor reads `capture_healthy()` from
        /// an async task. The health check must read around the held lock instead of stalling the
        /// whole supervisor; a held lock is an open in progress, not a failed recovery.
        #[test]
        fn health_check_reads_around_a_recovery_lock_held_by_an_open_in_progress() {
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let held = session.recovery.lock().unwrap();
            assert!(!session.recovery_failed(), "an in-progress open is not a failed recovery");
            drop(held);
            assert!(!session.recovery_failed());
        }

        #[test]
        fn health_check_still_reports_a_recorded_recovery_backoff() {
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            session.recovery.lock().unwrap().failed(std::time::Instant::now(), 3);
            assert!(session.recovery_failed());
            session.recovery.lock().unwrap().succeeded();
            assert!(!session.recovery_failed());
        }

        struct TestHandle {
            generation: u8,
            key_held: bool,
            button_held: bool,
            retirement: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
        }
        #[test]
        fn frame_wait_lease_keeps_replaced_handle_alive_without_blocking_input() {
            use std::sync::{atomic::{AtomicBool, Ordering}, mpsc::channel};
            struct Tracked(Arc<AtomicBool>);
            impl Drop for Tracked { fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); } }
            let dropped = Arc::new(AtomicBool::new(false));
            let current = Arc::new(Mutex::new(Arc::new(Tracked(dropped.clone()))));
            let (waiting, ready) = channel();
            let (resume, resumed) = channel();
            let worker = {
                let current = current.clone();
                std::thread::spawn(move || {
                    let lease = lease_handle(&current).unwrap();
                    waiting.send(()).unwrap();
                    resumed.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
                    assert!(!lease.0.load(Ordering::SeqCst));
                })
            };
            ready.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
            let old = {
                let mut guard = current.try_lock().expect("frame wait must not block input");
                std::mem::replace(&mut *guard, Arc::new(Tracked(Arc::new(AtomicBool::new(false)))))
            };
            drop(old);
            assert!(!dropped.load(Ordering::SeqCst));
            resume.send(()).unwrap();
            worker.join().unwrap();
            assert!(dropped.load(Ordering::SeqCst));
        }

        #[test]
        fn queued_recover_is_cancelled_when_desktop_ends_while_waiting_on_lock() {
            use std::{sync::{atomic::{AtomicBool, Ordering}, mpsc::channel}, time::Duration};
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let (entered, queued) = channel();
            let (done_tx, done_rx) = channel();
            std::thread::scope(|scope| {
                let held = session.recovery.lock().unwrap();
                let worker = scope.spawn(move || {
                    entered.send(()).unwrap();
                    let cancel = AtomicBool::new(false);
                    let res = session.recover(&cancel);
                    let _ = done_tx.send(res);
                });
                queued.recv_timeout(Duration::from_secs(2)).unwrap();
                // Give recover() a moment to snapshot end_generation and block on self.recovery.
                std::thread::sleep(Duration::from_millis(40));
                // Simulate end_idle() -> Session::end() even when the previous handle is already dead.
                session.end();
                let res = done_rx.recv_timeout(Duration::from_secs(2)).expect("queued recover must cancel on end");
                assert_eq!(res.unwrap_err().code, 13);
                drop(held);
                worker.join().unwrap();
            });
            assert!(session.ended.load(Ordering::Acquire));
            // Any recover() entered AFTER end() must also be rejected until begin() re-arms it.
            assert_eq!(session.recover(&AtomicBool::new(false)).unwrap_err().code, 13);
            session.begin();
            assert!(!session.ended.load(Ordering::Acquire));
        }

        #[test]
        fn end_waits_for_handle_lock_and_preserves_ended_state_after_concurrent_swap() {
            use std::{sync::{atomic::Ordering, mpsc::channel}, time::Duration};
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let (locked_tx, locked_rx) = channel();
            let (ended_tx, ended_rx) = channel();
            std::thread::scope(|scope| {
                // Hold `self.handle` just like `recover()` right after its final cancellation check.
                let handle_guard = session.handle.lock().unwrap();
                locked_tx.send(()).unwrap();
                let ender = scope.spawn(move || {
                    locked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    session.end();
                    ended_tx.send(()).unwrap();
                });
                // Give `end()` time to block on `self.handle`. While the swap holds `self.handle`,
                // `ended` must not yet have been flipped out from under the swap.
                std::thread::sleep(Duration::from_millis(40));
                assert!(!session.ended.load(Ordering::Acquire));
                drop(handle_guard);
                ended_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                ender.join().unwrap();
            });
            // Once `end()` completes after the swap, `ended` stays true and blocks subsequent recover().
            assert!(session.ended.load(Ordering::Acquire));
            assert_eq!(session.recover(&std::sync::atomic::AtomicBool::new(false)).unwrap_err().code, 13);
        }

        #[test]
        fn install_replacement_rejects_after_end_and_disable_dmabuf_persists() {
            use std::sync::atomic::Ordering;
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let old_gen = session.end_generation.load(Ordering::Acquire);
            // Simulate end_idle() firing while upgrade_touch / switch_to / set_virtual waited on consent.
            session.end();
            // Even with the post-end generation, `ended == true` blocks installation until begin().
            let post_gen = session.end_generation.load(Ordering::Acquire);
            let err = session.install_replacement(Session::test_dead(), post_gen, false, false).unwrap_err();
            assert_eq!(err, "desktop session ended during the rebuild");
            // After begin() clears `ended`, an opener carrying the pre-end generation must STILL
            // be rejected purely by generation mismatch.
            session.begin();
            let err = session.install_replacement(Session::test_dead(), old_gen, false, false).unwrap_err();
            assert_eq!(err, "desktop session ended during the rebuild");
            // A fresh opener carrying the post-end generation succeeds once active again.
            assert_eq!(session.install_replacement(Session::test_dead(), post_gen, false, false).unwrap(), (1920, 1080));

            // Per-connection SHM preference routes VAAPI to host upload (`Cpu`) during this
            // connection (and on the same live SHM handle across a fast takeover) without
            // returning `Unavailable` / setting sticky `dma_failed`. Once the handle ends and a
            // new desktop connection calls `begin()`, DMA-BUF is re-evaluated; sticky
            // `disable_dmabuf()` survives `end()` + `begin()`.
            assert!(session.allow_dmabuf());
            session.use_shm_capture();
            assert!(!session.allow_dmabuf());
            assert!(matches!(session.with_dmabuf(&mut None, |_, _| Ok(())).unwrap(), super::super::DmaCapture::Cpu));
            session.begin();
            assert!(session.allow_dmabuf(), "a new desktop connection must re-evaluate DMA-BUF after a normal CPU/SHM session");
            assert!(matches!(session.with_dmabuf(&mut None, |_, _| Ok(())).unwrap(), super::super::DmaCapture::Cpu),
                "a still-live SHM handle must route to host upload instead of tripping sticky Unavailable");
            session.end();
            session.begin();
            let after_reset = session.with_dmabuf(&mut None, |_, _| Ok(())).unwrap();
            if Session::dmabuf_abandon_supported() {
                assert!(matches!(after_reset, super::super::DmaCapture::Pending),
                    "after closing the SHM handle and beginning a new session, DMA-BUF is attempted again");
            } else {
                assert!(matches!(after_reset, super::super::DmaCapture::Unavailable(14, _)),
                    "an older runtime plugin without uu_portal_dmabuf_abandon must refuse DMA-BUF");
            }
            session.disable_dmabuf();
            assert!(!session.allow_dmabuf());
            assert!(matches!(session.with_dmabuf(&mut None, |_, _| Ok(())).unwrap(), super::super::DmaCapture::Unavailable(14, _)));
            session.end();
            session.begin();
            assert!(!session.allow_dmabuf(), "an actual DMA-BUF failure must stay disabled across reconnects");
            assert!(matches!(session.with_dmabuf(&mut None, |_, _| Ok(())).unwrap(), super::super::DmaCapture::Unavailable(14, _)));
        }

        #[test]
        fn dmabuf_acquire_racing_end_shm_or_swap_does_not_poison_dmabuf() {
            use std::sync::atomic::Ordering;
            // On engine-plugins builds with an older host plugin lacking uu_portal_dmabuf_abandon,
            // `with_dmabuf_with_acquire` refuses before reaching the C acquire closure.
            if !Session::dmabuf_abandon_supported() { return; }

            fn run_interleaving(
                session: &Session,
                expected_size: Option<(u32, u32)>,
                version: &mut Option<super::super::FrameVersion>,
                code: i32,
                race: impl FnOnce(),
            ) -> super::super::DmaCapture<()> {
                use std::{sync::mpsc::channel, time::Duration};
                let (entered_tx, entered_rx) = channel();
                let (resume_tx, resume_rx) = channel();
                std::thread::scope(|scope| {
                    let worker = scope.spawn(move || {
                        session.with_dmabuf_with_acquire(
                            expected_size,
                            version,
                            |_handle, _frame, _rev| {
                                entered_tx.send(()).unwrap();
                                resume_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                                code
                            },
                            |_, _, _, _| panic!("failed acquire must not invoke encode"),
                        ).unwrap()
                    });
                    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                    race();
                    resume_tx.send(()).unwrap();
                    worker.join().unwrap()
                })
            }

            let acquire_live_frame = |session: &Session, size: (u32, u32)| {
                let mut v = None;
                let got = session.with_dmabuf_with_acquire(
                    Some(size),
                    &mut v,
                    |_handle, frame, rev| {
                        frame.width = size.0;
                        frame.height = size.1;
                        *rev = 1;
                        0
                    },
                    |frame, changed, _id, _abandon| Ok((frame.width, frame.height, changed)),
                ).unwrap();
                matches!(got, super::super::DmaCapture::Frame((w, h, true)) if (w, h) == size)
            };

            // 1. Racing `use_shm_capture()` in the gap before C acquire: C returns 14
            //    (`dma_blocked`), which must classify as `Cpu` (not `Unavailable`) and leave
            //    sticky `dma_failed == false`.
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let mut version = None;
            let outcome = run_interleaving(session, Some((1920, 1080)), &mut version, 14, || session.use_shm_capture());
            assert!(matches!(outcome, super::super::DmaCapture::Cpu), "racing SHM switch must return Cpu");
            assert!(!session.dma_failed.load(Ordering::Acquire));

            // 2. Racing `end()` + `begin()` before C returns 14 on the closed handle must classify
            //    as `Dead`, not `Unavailable`, leaving DMA-BUF enabled.
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let outcome = run_interleaving(session, Some((1920, 1080)), &mut version, 14, || {
                session.end();
                session.begin();
            });
            assert!(matches!(outcome, super::super::DmaCapture::Dead), "racing end()+begin() must return Dead");
            assert!(!session.dma_failed.load(Ordering::Acquire));
            assert!(session.allow_dmabuf());

            // 3. Old handle dies (`code == 9` -> `Dead`), then a same-size replacement handle is
            //    installed BEFORE the encoder runs `capture_dead_or_retry`: must NOT switch the new
            //    live handle to SHM, and the new handle must still acquire DMA-BUF frames.
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let gen = session.end_generation.load(Ordering::Acquire);
            let outcome = run_interleaving(session, Some((1920, 1080)), &mut version, 9, || {});
            assert!(matches!(outcome, super::super::DmaCapture::Dead), "closed stream (code 9) must return Dead");
            session.install_replacement(Session::test_dead_with_size(1920, 1080), gen, false, false).unwrap();
            let mut buf = vec![0u8; 1920 * 1080 * 4];
            let retry = session.capture_changed_inner(&mut buf, &mut None, false, |_handle| true).unwrap();
            assert!(retry.is_none(), "live replacement handle must retry DMA-BUF instead of switching to SHM");
            assert!(!session.dma_shm_only.load(Ordering::Acquire));
            assert!(session.allow_dmabuf());
            assert!(acquire_live_frame(session, (1920, 1080)), "new handle must still acquire DMA-BUF");

            // 4. Racing handle replacement (`install_replacement`) during acquire must classify as
            //    `Pending`, clear stale `version`, and keep DMA-BUF enabled for the new handle.
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let gen = session.end_generation.load(Ordering::Acquire);
            version = Some(super::super::FrameVersion(123, 7));
            let outcome = run_interleaving(session, Some((1920, 1080)), &mut version, 14, || {
                session.install_replacement(Session::test_dead_with_size(1920, 1080), gen, false, false).unwrap();
            });
            assert!(matches!(outcome, super::super::DmaCapture::Pending), "retired handle must retry as Pending");
            assert!(version.is_none());
            assert!(!session.dma_failed.load(Ordering::Acquire));
            assert!(acquire_live_frame(session, (1920, 1080)));

            // 5. After the encoder's outer size check passed, a swap to a different-size handle
            //    (1920x1080 -> 3072x1920) produces a 3072x1920 frame during acquire: must return
            //    `SizeChanged` WITHOUT invoking `encode` (so import never fails) or disabling
            //    DMA-BUF, and a restarted 3072x1920 encoder acquires DMA-BUF on the new handle.
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let gen = session.end_generation.load(Ordering::Acquire);
            let mut v = None;
            let outcome: super::super::DmaCapture<()> = session.with_dmabuf_with_acquire(
                Some((1920, 1080)),
                &mut v,
                |_handle, frame, rev| {
                    session.install_replacement(Session::test_dead_with_size(3072, 1920), gen, false, false).unwrap();
                    frame.width = 3072;
                    frame.height = 1920;
                    *rev = 1;
                    0
                },
                |_, _, _, _| panic!("size mismatch must abort before VAAPI import"),
            ).unwrap();
            assert!(matches!(outcome, super::super::DmaCapture::SizeChanged));
            assert!(!session.dma_failed.load(Ordering::Acquire));
            assert!(acquire_live_frame(session, (3072, 1920)), "new physical handle must acquire DMA-BUF");

            // 6. Genuine `Unavailable(14, Some(old_id))` on `h1` must NOT poison `h2` if `h2` was
            //    installed before the encoder executes `disable_dmabuf_for(Some(old_id))`; calling
            //    `disable_dmabuf_for(Some(current_id))` on the still-active handle DOES disable it.
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let gen = session.end_generation.load(Ordering::Acquire);
            let outcome = run_interleaving(session, Some((1920, 1080)), &mut version, 14, || {});
            let old_id = match outcome {
                super::super::DmaCapture::Unavailable(14, Some(id)) => id,
                _ => panic!("genuine failure on live handle must carry handle id"),
            };
            session.install_replacement(Session::test_dead_with_size(1920, 1080), gen, false, false).unwrap();
            assert!(!session.disable_dmabuf_for(Some(old_id)), "stale handle degradation must not disable replacement");
            assert!(!session.dma_failed.load(Ordering::Acquire));
            assert!(acquire_live_frame(session, (1920, 1080)), "replacement handle must still acquire DMA-BUF");
            let current_id = session.handle.lock().unwrap().1;
            assert!(session.disable_dmabuf_for(Some(current_id)), "active handle failure must disable DMA-BUF");
            assert!(session.dma_failed.load(Ordering::Acquire));
        }

        #[test]
        fn set_virtual_false_clears_virtual_output_when_desktop_is_idle() {
            use std::{sync::{atomic::Ordering, mpsc::channel}, time::Duration};
            let fixture = Fixture::new((1920, 1080));
            let session = fixture.session();
            let physical = Fixture::new((3072, 1920));
            // Without an idle privacy exit, recovery rejects mismatched physical dimensions.
            assert_eq!(session.check_recovery_geometry(physical.session()).unwrap_err().code, 14);

            session.virtual_output.store(true, Ordering::Release);
            session.end();
            // `set_virtual(false)` must serialize on `self.recovery` instead of bypassing it.
            let (done_tx, done_rx) = channel();
            std::thread::scope(|scope| {
                let held = session.recovery.lock().unwrap();
                let worker = scope.spawn(move || {
                    let res = session.set_virtual(false);
                    let _ = done_tx.send(res);
                });
                assert!(done_rx.recv_timeout(Duration::from_millis(40)).is_err(),
                    "idle set_virtual(false) must wait for the recovery lock");
                assert!(session.is_virtual());
                drop(held);
                assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap().is_ok());
                worker.join().unwrap();
            });
            assert!(!session.is_virtual());
            assert!(session.geometry_pending.load(Ordering::Acquire));
            // The next reconnect accepts the physical monitor's dimensions (3072x1920).
            assert_eq!(session.check_recovery_geometry(physical.session()).unwrap(), Some((3072, 1920)));
            // Entering privacy screen while idle must still be rejected.
            assert!(session.set_virtual(true).is_err());
            assert!(!session.is_virtual());
        }

        #[test]
        fn rejected_replacement_drops_after_releasing_clipboard_and_input_locks() {
            use std::sync::atomic::{AtomicBool, Ordering};
            struct DropCheck {
                clipboard: Arc<Mutex<()>>,
                dropped: Arc<AtomicBool>,
                clipboard_free_on_drop: Arc<AtomicBool>,
                input_free_on_drop: Arc<AtomicBool>,
            }
            impl Drop for DropCheck {
                fn drop(&mut self) {
                    self.clipboard_free_on_drop.store(self.clipboard.try_lock().is_ok(), Ordering::Release);
                    // Attempting to take the input held lock on a second thread proves this thread
                    // no longer holds it when Drop (D-Bus Close) runs.
                    let free = std::thread::spawn(|| {
                        let _g = crate::input::lock_portal_held();
                        true
                    }).join().unwrap_or(false);
                    self.input_free_on_drop.store(free, Ordering::Release);
                    self.dropped.store(true, Ordering::Release);
                }
            }
            let clipboard = Arc::new(Mutex::new(()));
            let current_dropped = Arc::new(AtomicBool::new(false));
            let current = Mutex::new(Arc::new(DropCheck {
                clipboard: clipboard.clone(),
                dropped: current_dropped.clone(),
                clipboard_free_on_drop: Arc::new(AtomicBool::new(false)),
                input_free_on_drop: Arc::new(AtomicBool::new(false)),
            }));
            let cand_dropped = Arc::new(AtomicBool::new(false));
            let clip_free = Arc::new(AtomicBool::new(false));
            let input_free = Arc::new(AtomicBool::new(false));
            let locked = current.lock().unwrap();
            let swapped = replace_handle_guarded(
                &current,
                &clipboard,
                locked,
                || Ok(Some(Arc::new(DropCheck {
                    clipboard: clipboard.clone(),
                    dropped: cand_dropped.clone(),
                    clipboard_free_on_drop: clip_free.clone(),
                    input_free_on_drop: input_free.clone(),
                }))),
                || false,
                || panic!("rejected candidate must not run on_installed"),
                |_, _| panic!("rejected candidate must not copy clipboard"),
                || panic!("rejected candidate must not clear input"),
            ).unwrap();
            assert!(!swapped);
            assert!(cand_dropped.load(Ordering::Acquire), "rejected candidate must be dropped");
            assert!(clip_free.load(Ordering::Acquire), "clipboard lock must be released before candidate drop");
            assert!(input_free.load(Ordering::Acquire), "input held lock must be released before candidate drop");
            assert!(!current_dropped.load(Ordering::Acquire), "existing handle must stay installed");
        }
        impl Drop for TestHandle {
            fn drop(&mut self) {
                if let Some((entered, resume)) = self.retirement.take() {
                    let _ = entered.send(());
                    let _ = resume.recv_timeout(std::time::Duration::from_secs(10));
                }
            }
        }
        fn release_input(current: Arc<Mutex<TestHandle>>) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Receiver<(u8, bool, bool)>) {
            let (released, received) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut handle = current.lock().unwrap();
                handle.key_held = false;
                handle.button_held = false;
                released.send((handle.generation, handle.key_held, handle.button_held)).unwrap();
            });
            (worker, received)
        }

        #[test]
        fn recovery_does_not_block_input_releases_during_open_or_close() {
            use std::{sync::mpsc::channel, time::Duration};
            let timeout = Duration::from_secs(3);
            let (opening, opened) = channel();
            let (resume_open, wait_open) = channel();
            let (closing, closed) = channel();
            let (resume_close, wait_close) = channel();
            let current = Arc::new(Mutex::new(TestHandle {
                generation: 1, key_held: true, button_held: true, retirement: Some((closing, wait_close)),
            }));
            let retry = Arc::new(Mutex::new(None::<std::time::Instant>));
            let recovery = {
                let current = current.clone();
                let retry = retry.clone();
                std::thread::spawn(move || {
                    let handle = current.lock().unwrap();
                    let _owner = retry.try_lock().unwrap();
                    replace_handle(&current, handle, || {
                        opening.send(()).unwrap();
                        wait_open.recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(Some(TestHandle {
                            generation: 2, key_held: true, button_held: true, retirement: None,
                        }))
                    })
                })
            };
            let opening_result = opened.recv_timeout(timeout);
            let (first_worker, first_release) = release_input(current.clone());
            let first_result = first_release.recv_timeout(timeout);
            // A concurrent capture must not wait on recovery while retaining input's mutex.
            let (contended, contention) = channel();
            let contender = {
                let current = current.clone();
                let retry = retry.clone();
                std::thread::spawn(move || {
                    let _handle = current.lock().unwrap();
                    contended.send(matches!(retry.try_lock(), Err(TryLockError::WouldBlock))).unwrap();
                })
            };
            let contention_result = contention.recv_timeout(timeout);
            let _ = resume_open.send(());
            let closing_result = closed.recv_timeout(timeout);
            let (second_worker, second_release) = release_input(current.clone());
            let second_result = second_release.recv_timeout(timeout);
            // Always release blocked work before asserting, including on regression.
            let _ = resume_close.send(());
            let recovered = recovery.join().unwrap();
            first_worker.join().unwrap();
            second_worker.join().unwrap();
            contender.join().unwrap();
            assert!(opening_result.is_ok() && closing_result.is_ok());
            assert_eq!(first_result.unwrap(), (1, false, false));
            assert_eq!(second_result.unwrap(), (2, false, false));
            assert!(contention_result.unwrap());
            assert!(recovered.unwrap());
        }

        #[test]
        fn failed_or_deferred_recovery_keeps_the_current_handle_unlocked() {
            let current = Mutex::new(1);
            for failure in [false, true] {
                let handle = current.lock().unwrap();
                let result = replace_handle(&current, handle, || {
                    assert_eq!(*current.try_lock().unwrap(), 1);
                    if failure { Err("open failed".into()) } else { Ok(None) }
                });
                if failure { assert_eq!(result.unwrap_err(), "open failed"); }
                else { assert!(!result.unwrap()); }
                assert_eq!(*current.try_lock().unwrap(), 1);
            }
        }

        #[test]
        fn terminal_failure_stops_encoder_recovery_until_the_supervisor_succeeds() {
            let now = std::time::Instant::now();
            let later = now + std::time::Duration::from_secs(60);
            let mut recovery = Recovery::default();
            recovery.failed(now, 3);
            assert!(!recovery.denied && recovery.failed_until.is_some());
            recovery.failed(now, 4);
            assert!(recovery.denied);
            assert!(!recovery.ready(later, || panic!("denied consent must not be probed again")));
            recovery.succeeded();
            assert!(!recovery.denied && recovery.failed_until.is_none() && recovery.retry_at.is_none());
        }

        #[test]
        fn recovery_backoff_skips_lock_probes_until_the_deadline() {
            use std::{cell::Cell, time::{Duration, Instant}};
            let now = Instant::now();
            let probes = Cell::new(0);
            let unlocked = || { probes.set(probes.get() + 1); true };
            let mut recovery = Recovery::default();
            // Initial one-second grace period, polled at up to 144 fps.
            for millis in 0..1000 {
                assert!(!recovery.ready(now + Duration::from_millis(millis), unlocked));
            }
            assert_eq!(probes.get(), 0);
            assert!(recovery.ready(now + Duration::from_secs(1), unlocked));
            assert_eq!(probes.get(), 1);
            // A failed open schedules the existing five-second retry delay.
            recovery.retry_at = Some(now + Duration::from_secs(6));
            for millis in 1000..6000 {
                assert!(!recovery.ready(now + Duration::from_millis(millis), unlocked));
            }
            assert_eq!(probes.get(), 1);
            // A locked/unknown probe denies opening and is itself throttled.
            assert!(!recovery.ready(now + Duration::from_secs(6), || { probes.set(2); false }));
            assert!(!recovery.ready(now + Duration::from_millis(6999), || panic!("early lock probe")));
            assert!(recovery.ready(now + Duration::from_secs(7), unlocked));
            assert_eq!(probes.get(), 3);
        }

        #[test]
        fn lock_observation_survives_unlock_while_recovery_is_opening() {
            use std::{sync::{atomic::Ordering, mpsc::channel}, time::Duration};
            let mut fixture = Fixture::new((2, 2));
            let (locked, watch) = tokio::sync::watch::channel(Some(false));
            fixture.0.locked = watch;
            let session = fixture.session();
            std::thread::scope(|threads| {
                let (opening, opened) = channel();
                let (resume, wait) = channel();
                let recovery = threads.spawn(move || {
                    let _owner = session.recovery.lock().unwrap();
                    opening.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(10)).unwrap();
                });
                let ready = opened.recv_timeout(Duration::from_secs(3));
                locked.send_replace(Some(true));
                session.was_locked.store(true, Ordering::Release);
                locked.send_replace(Some(false));
                let (reset, result) = channel();
                let capture = threads.spawn(move || {
                    let _handle = session.handle.lock().unwrap();
                    assert_eq!(*session.locked.borrow(), Some(false));
                    reset.send(session.take_lock_observation()).unwrap();
                });
                let consumed = result.recv_timeout(Duration::from_secs(3));
                let still_pending = session.was_locked.load(Ordering::Acquire);
                let _ = resume.send(());
                recovery.join().unwrap();
                capture.join().unwrap();
                assert!(ready.is_ok());
                assert!(!consumed.unwrap().unwrap());
                assert!(still_pending);
            });
            assert!(session.take_lock_observation().unwrap());
            assert!(!session.was_locked.load(std::sync::atomic::Ordering::Acquire));
        }

        #[test]
        fn cancelled_recovery_does_not_touch_the_handle() {
            let session = Fixture::new((1920, 1080));
            let cancelled = std::sync::atomic::AtomicBool::new(true);
            assert_eq!(session.session().recover(&cancelled).unwrap_err().code, 13);
        }

        #[test]
        fn unlocked_replacement_keeps_its_first_static_frame() {
            let was_locked = std::sync::atomic::AtomicBool::new(true);
            clear_pause_for_unlocked_replacement(&was_locked, Some(false));
            assert!(!was_locked.load(std::sync::atomic::Ordering::Acquire));
        }
    }
}

fn blank(frame: &mut [u8]) { for pixel in frame.chunks_exact_mut(4) { pixel.copy_from_slice(&[0, 0, 0, 255]); } }

fn open_then_install<T>(
    cancelled: &std::sync::atomic::AtomicBool,
    open: impl FnOnce() -> Result<T, PortalError>,
    install: impl FnOnce(T) -> Result<(), PortalError>,
) -> Result<(), PortalError> {
    open_then_install_when(|| cancelled.load(std::sync::atomic::Ordering::Acquire), open, install)
}

fn open_then_install_when<T>(
    cancelled: impl Fn() -> bool,
    open: impl FnOnce() -> Result<T, PortalError>,
    install: impl FnOnce(T) -> Result<(), PortalError>,
) -> Result<(), PortalError> {
    if cancelled() { return Err(PortalError { code: 13 }); }
    let replacement = open()?;
    if cancelled() { return Err(PortalError { code: 13 }); }
    install(replacement)
}

fn error(code: i32) -> String {
    let reason = match code {
        1 => "session bus unavailable", 2 => "RemoteDesktop/ScreenCast portal unavailable",
        3 => "request failed", 4 => "permission cancelled or denied", 5 => "permission request timed out",
        6 => "single monitor and keyboard/pointer permission required", 7 => "PipeWire connection failed",
        8 => "unsupported frame format or size", 9 => "session closed or stream lost",
        10 => "frame unavailable", 11 => "invalid input or frame buffer",
        13 => "operation cancelled", 14 => "dimensions changed; reconnect required",
        15 => return match crate::engine_plugin::load(crate::engine_plugin::Engine::Portal) {
            Err(reason) if cfg!(feature = "portal") => format!("GNOME/KDE Wayland (portal) engine unavailable: {reason}"),
            _ => NOT_BUILT.into(),
        },
        _ => "operation failed",
    };
    format!("Portal {reason} (code {code})")
}
fn check(code: i32) -> Result<(), String> { if code == 0 { Ok(()) } else { Err(error(code)) } }

/// GNOME/KDE Wayland selected the portal backend, but this binary was built without it (the release
/// package is X11 only): say how to get a picture instead of an opaque "portal unavailable".
pub const NOT_BUILT: &str = "this build has no GNOME/KDE Wayland (portal) support: choose the Xorg session \
    (\"GNOME on Xorg\" / \"Plasma (X11)\") at the login screen, or build with --features wayland,portal";
#[cfg(all(target_os = "linux", feature = "portal"))]
pub use native::Session as PortalSession;
#[cfg(not(all(target_os = "linux", feature = "portal")))]
pub struct PortalSession { pub width: u32, pub height: u32 }
#[cfg(not(all(target_os = "linux", feature = "portal")))]
impl PortalSession {
    pub fn size(&self) -> (u32, u32) { (self.width, self.height) }
    pub fn monitor(&self) -> Option<String> { None }
    pub fn begin(&self) {}
    pub fn end(&self) {}
    pub fn switch_to(&self, _: &str) -> Result<(), String> { Err(error(15)) }
    pub fn set_virtual(&self, _: bool) -> Result<(), String> { Err(error(15)) }
    pub fn set_cursor_embedded(&self, _: bool) -> Result<bool, String> { Err(error(15)) }
    pub fn is_virtual(&self) -> bool { false }
}
#[cfg(not(all(target_os = "linux", feature = "portal")))]
impl PortalSession {
    pub fn clipboard_supported(&self) -> bool { false }
    pub fn clipboard_ready(&self) -> bool { false }
    pub fn clipboard_set(&self, _: &str) -> Result<(), String> { Err(error(15)) }
    pub fn clipboard_poll(&self) -> Option<String> { None }
    pub fn is_alive(&self) -> bool { false }
    pub fn capture_healthy(&self) -> bool { false }
    pub fn open() -> Result<Self, String> { Err("Portal backend requires Linux and --features portal".into()) }
    pub fn open_cancellable(cancelled: &std::sync::atomic::AtomicBool) -> Result<Self, PortalError> {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) { Err(PortalError { code: 13 }) }
        else { Err(PortalError { code: 15 }) }
    }
    pub fn recover(&self, cancelled: &std::sync::atomic::AtomicBool) -> Result<bool, PortalError> {
        if cancelled.load(std::sync::atomic::Ordering::Acquire) { Err(PortalError { code: 13 }) }
        else { Err(PortalError { code: 15 }) }
    }
    pub fn pause(&self) {}
    pub fn capture(&self, _: &mut [u8]) -> Result<(), String> { Err(error(15)) }
    pub fn capture_changed(&self, _: &mut [u8], _: &mut Option<FrameVersion>) -> Result<bool, String> { Err(error(15)) }
    pub fn capture_dead_or_retry(&self, _: &mut [u8], _: &mut Option<FrameVersion>) -> Result<Option<bool>, String> { Err(error(15)) }
    pub fn disable_dmabuf(&self) {}
    pub fn disable_dmabuf_for(&self, _: Option<u64>) -> bool { false }
    pub fn use_shm_capture(&self) {}
    pub fn wait_frame(&self, _: &mut u64, _: std::time::Duration) -> Result<bool, String> { Err(error(15)) }
    pub fn with_dmabuf<T>(&self, _: &mut Option<FrameVersion>, _: impl FnOnce(&DmaFrame, bool) -> Result<T, String>) -> Result<DmaCapture<T>, String> {
        Ok(DmaCapture::Unavailable(-1, None))
    }
    pub fn with_dmabuf_controlled<T>(&self, _: Option<(u32, u32)>, _: &mut Option<FrameVersion>, _: impl FnOnce(&DmaFrame, bool, u64, &mut bool) -> Result<T, String>) -> Result<DmaCapture<T>, String> {
        Ok(DmaCapture::Unavailable(-1, None))
    }
    pub fn move_pointer(&self, _: f64, _: f64, _: bool) -> Result<(), String> { Err(error(15)) }
    pub fn button(&self, _: i32, _: bool) -> Result<(), String> { Err(error(15)) }
    pub fn touch_supported(&self) -> bool { false }
    pub fn scroll(&self, _: f64, _: f64, _: bool) -> Result<(), String> { Err(error(15)) }
    pub fn upgrade_touch(&self) -> Result<bool, String> { Err(error(15)) }
    pub fn type_keysym(&self, _: u32) -> Result<(), String> { Err(error(15)) }
    pub fn touch(&self, _: i32, _: f64, _: f64) -> Result<(), String> { Err(error(15)) }
    pub fn key(&self, _: u64, _: bool) -> Result<(), String> { Err(error(15)) }
}

/// The portal session that monitor switches act on: one per host, reused by reconnects.
static ACTIVE: std::sync::Mutex<Option<std::sync::Weak<PortalSession>>> = std::sync::Mutex::new(None);
pub fn permissions_directory() -> Option<std::path::PathBuf> { permissions::directory() }
pub fn monitor_key(connector: &str) -> String { permissions::monitor_key(connector) }
pub fn permission_exists(dir: &std::path::Path, key: &str) -> bool { permissions::exists(dir, key) }
#[cfg(unix)]
pub fn remembered_clipboard_granted() -> bool {
    permissions::existing_directory().is_some_and(|dir| permissions::clipboard_granted(&dir, permissions::desktop_key()))
}
#[cfg(not(unix))]
pub fn remembered_clipboard_granted() -> bool { false }
pub fn register(session: &Arc<PortalSession>) {
    *ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(session));
}
/// Whether a desktop capture session is registered for monitor/virtual switching.
pub fn available() -> bool {
    ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().and_then(std::sync::Weak::upgrade).is_some()
}
/// Privacy screen: see `Session::set_virtual`. Restoring without a live session is a no-op;
/// failures switching a live session must reach the controller so its recovery record survives.
pub fn set_virtual_active(on: bool) -> Result<(), String> {
    let session = ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().and_then(std::sync::Weak::upgrade);
    let Some(session) = session else { return if on { Err("Portal desktop unavailable".into()) } else { Ok(()) }; };
    session.set_virtual(on)
}
pub fn switch_active(connector: &str) -> Result<(), String> {
    let session = ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().and_then(std::sync::Weak::upgrade)
        .ok_or("Portal desktop unavailable")?;
    session.switch_to(connector)
}
/// Whether the capture stream carries the compositor's pointer. 0 is "not asked yet"; the
/// startup answer has to match what the engine reads from UU_PORTAL_CURSOR_MODE.
static CURSOR_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
/// Embedded cursor support reported by a session: 0 unknown, 1 unavailable, 2 available.
static CURSOR_EMBED: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
fn cursor_embedded() -> bool {
    use std::sync::atomic::Ordering;
    match CURSOR_MODE.load(Ordering::Acquire) {
        0 => {
            let embedded = std::env::var("UU_PORTAL_CURSOR_MODE").is_ok_and(|value| value.eq_ignore_ascii_case("embedded"));
            CURSOR_MODE.store(if embedded { 2 } else { 1 }, Ordering::Release);
            embedded
        }
        mode => mode == 2,
    }
}
/// Ask the running session for a stream with (or without) the compositor cursor.
pub fn set_cursor_embedded(embedded: bool) -> Result<bool, String> {
    let session = ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().and_then(std::sync::Weak::upgrade)
        .ok_or("Portal desktop unavailable")?;
    session.set_cursor_embedded(embedded)
}
/// A release package can carry an older libuu-portal.so without the runtime cursor switch;
/// rebuilding the session would be wasted work there.
fn cursor_switch_available() -> bool {
    #[cfg(all(target_os = "linux", feature = "portal", feature = "engine-plugins"))]
    {
        static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *AVAILABLE.get_or_init(|| crate::engine_plugin::symbol(crate::engine_plugin::Engine::Portal, "uu_portal_set_cursor_mode\0") != 0)
    }
    #[cfg(not(all(target_os = "linux", feature = "portal", feature = "engine-plugins")))]
    { cfg!(all(target_os = "linux", feature = "portal")) }
}
/// Relative motion has no position the viewer could draw a cursor at, so the stream has to carry
/// one. Absolute motion means the viewer draws its own and an embedded copy would trail it. Both
/// need a fresh portal session, so wait for a run of the same kind before rebuilding it.
struct PointerMode {
    candidate: Option<bool>,
    streak: u32,
    first: Option<std::time::Instant>,
    switching: bool,
    blocked_until: Option<std::time::Instant>,
}
static POINTER_MODE: std::sync::Mutex<PointerMode> = std::sync::Mutex::new(PointerMode {
    candidate: None, streak: 0, first: None, switching: false, blocked_until: None,
});
const POINTER_STREAK: u32 = 5;
/// A burst at connect must not rebuild the session, so the run has to span time as well.
const POINTER_WINDOW: std::time::Duration = std::time::Duration::from_millis(200);
/// After a failed rebuild the consent may be gone; do not retry on the next few events.
const POINTER_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);
fn pointer_decision(state: &mut PointerMode, current: bool, relative: bool, now: std::time::Instant) -> Option<bool> {
    if current == relative {
        state.candidate = None;
        state.streak = 0;
        state.first = None;
        return None;
    }
    if state.switching || state.blocked_until.is_some_and(|until| now < until) { return None; }
    match state.candidate {
        Some(candidate) if candidate == relative => {
            state.streak += 1;
            if state.streak >= POINTER_STREAK
                && state.first.is_some_and(|first| now.duration_since(first) >= POINTER_WINDOW) {
                return Some(relative);
            }
        }
        _ => {
            state.candidate = Some(relative);
            state.streak = 1;
            state.first = Some(now);
        }
    }
    None
}
pub fn note_pointer_motion(relative: bool) {
    if !cursor_switch_available() { return; }
    // The backend does not advertise an embedded cursor: nothing to switch to.
    if CURSOR_EMBED.load(std::sync::atomic::Ordering::Acquire) == 1 { return; }
    // A swap closes the portal input devices; do not drop keys or buttons the viewer holds.
    if crate::input::portal_input_held() { return; }
    let decision = {
        let Ok(mut state) = POINTER_MODE.lock() else { return; };
        let decision = pointer_decision(&mut state, cursor_embedded(), relative, std::time::Instant::now());
        if decision.is_some() {
            state.switching = true;
            state.candidate = None;
            state.streak = 0;
            state.first = None;
        }
        decision
    };
    let Some(target) = decision else { return; };
    let spawned = std::thread::Builder::new().name("portal-cursor".into()).spawn(move || {
        let result = set_cursor_embedded(target);
        let mut state = POINTER_MODE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.switching = false;
        state.candidate = None;
        state.streak = 0;
        state.first = None;
        match result {
            Ok(true) => {
                state.blocked_until = None;
                drop(state);
                log::info!("Cursor mode follows the pointer: embedded={target}");
            }
            Ok(false) => {
                // Locked, or something else changed the mode first; the next motion retries.
                state.blocked_until = None;
                drop(state);
            }
            Err(error) => {
                state.blocked_until = Some(std::time::Instant::now() + POINTER_COOLDOWN);
                drop(state);
                log::warn!("Portal cursor switch failed: {error}");
            }
        }
    });
    if spawned.is_err() {
        if let Ok(mut state) = POINTER_MODE.lock() { state.switching = false; }
    }
}
pub struct Monitor { pub connector: String, pub width: u32, pub height: u32, pub x: i32, pub y: i32, pub primary: bool }
#[cfg(all(target_os = "linux", feature = "portal"))]
pub use native::monitors;
#[cfg(not(all(target_os = "linux", feature = "portal")))]
pub fn monitors() -> Vec<Monitor> { Vec::new() }
/// `connector\twidth\theight\tx\ty\tprimary` lines from the engine.
fn parse_monitors(text: &str) -> Vec<Monitor> {
    text.lines().filter_map(|line| {
        let mut f = line.split('\t');
        let connector = f.next()?.to_owned();
        let (width, height) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?);
        let (x, y, primary) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?, f.next()? == "1");
        (!connector.is_empty() && connector.len() <= 64 && connector.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
            .then_some(Monitor { connector, width, height, x, y, primary })
    }).collect()
}

/// Forward frames in-process into encoder stdin. Cancellation drops the Arc;
/// the caller keeps one owning Arc for all subsequent desktop reconnects.
pub fn write_frames(session: Arc<PortalSession>, mut output: impl std::io::Write, cancelled: &std::sync::atomic::AtomicBool) -> Result<(), String> {
    let (width, height) = session.size();
    let length = (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4)).ok_or("Portal frame too large")?;
    let mut frame = vec![0; length];
    let interval = std::time::Duration::from_nanos(1_000_000_000 / crate::video::FPS);
    while !cancelled.load(std::sync::atomic::Ordering::Acquire) {
        let started = std::time::Instant::now();
        session.capture(&mut frame)?;
        output.write_all(&frame).map_err(|_| "Portal encoder input closed")?;
        if let Some(left) = interval.checked_sub(started.elapsed()) { std::thread::sleep(left); }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn privacy_restore_without_a_desktop_is_a_noop_but_enable_fails() {
        assert!(super::set_virtual_active(false).is_ok());
        assert_eq!(super::set_virtual_active(true).unwrap_err(), "Portal desktop unavailable");
    }
    #[test]
    fn monitor_lines_parse_and_reject_unsafe_connectors() {
        let m = super::parse_monitors("eDP-1\t2880\t1800\t0\t0\t1\nHDMI-1\t2560\t1440\t1920\t0\t0\n../x\t1\t1\t0\t0\t0\n");
        assert_eq!(m.len(), 2);
        assert!(m[0].primary && m[0].connector == "eDP-1" && (m[1].width, m[1].x) == (2560, 1920));
    }
    #[test]
    fn locked_frames_hide_old_content() { let mut frame = [42; 8]; super::blank(&mut frame); assert_eq!(frame, [0,0,0,255,0,0,0,255]); }
    #[test]
    fn errors_never_include_remote_messages_or_identifiers() {
        assert!(super::check(4).unwrap_err().contains("cancelled or denied"));
        assert!(super::check(9).unwrap_err().contains("closed or stream lost"));
        // Portal builds name the missing runtime engine; other builds say how to get one.
        let unavailable = super::check(15).unwrap_err();
        assert!(unavailable.contains("--features wayland,portal") || (cfg!(feature = "portal") && unavailable.contains("engine unavailable")));
    }
    #[test]
    fn portal_error_display_is_fixed_and_sanitized() {
        assert_eq!(super::PortalError { code: 13 }.to_string(), "Portal operation cancelled (code 13)");
        assert_eq!(super::PortalError { code: 14 }.to_string(), "Portal dimensions changed; reconnect required (code 14)");
        assert_eq!(super::PortalError { code: 123 }.to_string(), "Portal operation failed (code 123)");
    }
    #[test]
    fn opening_with_pre_cancelled_flag_returns_cancellation_code() {
        let cancelled = std::sync::atomic::AtomicBool::new(true);
        let code = match super::PortalSession::open_cancellable(&cancelled) {
            Err(error) => error.code,
            Ok(_) => panic!("pre-cancelled portal open unexpectedly succeeded"),
        };
        assert_eq!(code, 13);
    }
    #[test]
    fn replacement_opens_before_installing_under_the_handle_lock() {
        let handle = std::sync::Mutex::new(7);
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        super::open_then_install(&cancelled, || {
            assert_eq!(*handle.try_lock().unwrap(), 7);
            Ok(11)
        }, |replacement| {
            *handle.lock().unwrap() = replacement;
            Ok(())
        }).unwrap();
        assert_eq!(*handle.lock().unwrap(), 11);
    }
}

#[cfg(test)]
mod pointer_mode_tests {
    use super::{pointer_decision, PointerMode, POINTER_COOLDOWN, POINTER_WINDOW};
    use std::time::{Duration, Instant};

    fn state() -> PointerMode {
        PointerMode { candidate: None, streak: 0, first: None, switching: false, blocked_until: None }
    }

    #[test]
    fn a_connect_burst_does_not_rebuild_the_session() {
        let mut state = state();
        let start = Instant::now();
        for step in 0..20 {
            assert_eq!(pointer_decision(&mut state, true, false, start + Duration::from_millis(step)), None);
        }
    }

    #[test]
    fn a_sustained_change_decides_once_the_run_spans_the_window() {
        let mut state = state();
        let start = Instant::now();
        let mut decided = None;
        for step in 0..40 {
            let now = start + Duration::from_millis(step * 10);
            if let Some(value) = pointer_decision(&mut state, true, false, now) {
                assert!(now.duration_since(start) >= POINTER_WINDOW);
                decided = Some(value);
                break;
            }
        }
        assert_eq!(decided, Some(false));
    }

    #[test]
    fn an_opposite_event_restarts_the_run() {
        let mut state = state();
        let start = Instant::now();
        for step in 0..10 { pointer_decision(&mut state, true, false, start + Duration::from_millis(step)); }
        pointer_decision(&mut state, true, true, start + Duration::from_millis(10));
        for step in 11..19 {
            assert_eq!(pointer_decision(&mut state, true, false, start + Duration::from_millis(step)), None);
        }
    }

    #[test]
    fn a_failed_switch_waits_out_the_cooldown() {
        let mut state = state();
        let start = Instant::now();
        state.blocked_until = Some(start + POINTER_COOLDOWN);
        for step in 0..40 {
            assert_eq!(pointer_decision(&mut state, true, false, start + Duration::from_millis(step * 10)), None);
        }
        let after = start + POINTER_COOLDOWN + Duration::from_millis(1);
        let mut decided = None;
        for step in 0..30 {
            let now = after + Duration::from_millis(step * 10);
            if let Some(value) = pointer_decision(&mut state, true, false, now) { decided = Some(value); break; }
        }
        assert_eq!(decided, Some(false));
    }

    #[test]
    fn a_switch_in_flight_ignores_more_events() {
        let mut state = state();
        let start = Instant::now();
        state.switching = true;
        for step in 0..40 {
            assert_eq!(pointer_decision(&mut state, true, false, start + Duration::from_millis(step * 10)), None);
        }
    }
}
