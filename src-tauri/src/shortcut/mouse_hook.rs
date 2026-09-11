//! Low-level mouse hook backend for Windows.
//!
//! Captures mouse button events (X1/X2/Middle) system-wide using
//! `SetWindowsHookExW(WH_MOUSE_LL, ...)`. Matched events are blocked
//! from reaching other applications.
//!
//! All binding bookkeeping and event queueing lives in the
//! platform-independent [`super::mouse_bindings::MouseRegistry`]; this module
//! only provides the Windows event source. The hook thread starts lazily —
//! when the first binding is registered or recording begins — and stops again
//! when the registry goes idle, so apps without mouse bindings never pay for
//! a system-wide hook. A failed install is retried on the next activation.
//!
//! ## Architecture
//!
//! ```text
//! ┌──────────────────┐  start/stop   ┌──────────────────────┐
//! │  MouseHookState   │ ────────────▶ │  Mouse Hook Thread   │
//! │  (any thread)     │               │                      │
//! │ - registry events │   events      │ - SetWindowsHookExW  │
//! │ - lazy lifecycle  │ ◀──────────── │ - WH_MOUSE_LL        │
//! │                   │  (registry)   │ - GetMessage loop    │
//! └──────────────────┘               └──────────────────────┘
//! ```

use log::{error, info};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, MSG,
    MSLLHOOKSTRUCT, WH_MOUSE_LL, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_QUIT, WM_XBUTTONDOWN,
    WM_XBUTTONUP,
};

use super::mouse_bindings::{modifiers, MouseButton, MouseEvent, MouseRegistry};

// ============================================================================
// Global state for hook callback
// ============================================================================

/// Registry consulted by the hook callback (decides what to block/forward
/// and owns the event queue). Must be global because the `SetWindowsHookExW`
/// callback is a C function pointer without user data.
static HOOK_REGISTRY: OnceLock<Mutex<Option<MouseRegistry>>> = OnceLock::new();

/// Thread ID for the hook thread (used for PostThreadMessage to stop it).
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// Set while the hook thread is installed, to avoid double start/stop races.
static HOOK_RUNNING: AtomicBool = AtomicBool::new(false);

// ============================================================================
// MouseHookState — public API
// ============================================================================

/// Windows mouse capture backend.
///
/// Holds the hook thread lifecycle; all binding bookkeeping lives in the
/// [`MouseRegistry`] it is attached to. Create it once in
/// `HandyKeysState::new()` — *before* the state is registered with
/// `app.manage()`. The old implementation looked the hook up through
/// `app.try_state()` at registration time, which returned `None` during
/// startup, so persisted mouse bindings silently failed to register until the
/// user re-recorded them.
pub struct MouseHookState {
    thread_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl MouseHookState {
    /// Attach the hook to a registry.
    ///
    /// The hook thread is not started here — it starts on demand, the first
    /// time the registry becomes active (a binding is registered or recording
    /// begins), and stops again when it goes idle.
    pub fn new(registry: &MouseRegistry) -> Self {
        // Publish the registry for the hook callback.
        let registry_lock = HOOK_REGISTRY.get_or_init(|| Mutex::new(None));
        if let Ok(mut guard) = registry_lock.lock() {
            *guard = Some(registry.clone());
        }

        let thread_handle: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));

        // Start/stop the hook lazily as the registry becomes (in)active.
        let handle_for_listener = Arc::clone(&thread_handle);
        registry.on_active_change(Arc::new(move |active: bool| {
            if active {
                start_hook_thread(&handle_for_listener);
            } else {
                stop_hook_thread(&handle_for_listener);
            }
        }));

        info!("Mouse hook attached (starts lazily)");

        Self { thread_handle }
    }

    /// Stop the hook thread immediately (used on shutdown).
    pub fn stop(&self) {
        stop_hook_thread(&self.thread_handle);
    }
}

impl Drop for MouseHookState {
    fn drop(&mut self) {
        self.stop();
        // Clear the published registry so a later instance starts clean.
        if let Some(lock) = HOOK_REGISTRY.get() {
            if let Ok(mut guard) = lock.lock() {
                *guard = None;
            }
        }
        info!("Mouse hook state dropped, hook stopped");
    }
}

// ============================================================================
// Hook thread lifecycle
// ============================================================================

fn start_hook_thread(handle: &Arc<Mutex<Option<JoinHandle<()>>>>) {
    if HOOK_RUNNING.swap(true, Ordering::SeqCst) {
        return; // Already running or starting
    }

    // Reap a previous (finished or failed) thread handle before spawning.
    if let Ok(mut guard) = handle.lock() {
        *guard = Some(thread::spawn(hook_thread_main));
    } else {
        HOOK_RUNNING.store(false, Ordering::SeqCst);
    }
}

fn stop_hook_thread(handle: &Arc<Mutex<Option<JoinHandle<()>>>>) {
    // Post WM_QUIT to the hook thread to exit its message loop.
    let thread_id = HOOK_THREAD_ID.load(Ordering::SeqCst);
    if thread_id != 0 {
        unsafe {
            let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }

    if let Ok(mut guard) = handle.lock() {
        if let Some(h) = guard.take() {
            let _ = h.join();
        }
    }

    HOOK_RUNNING.store(false, Ordering::SeqCst);
}

/// Main function for the hook thread.
fn hook_thread_main() {
    // Store thread ID so we can post WM_QUIT later.
    let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    HOOK_THREAD_ID.store(thread_id, Ordering::SeqCst);

    // Install the low-level mouse hook.
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) };

    let hook = match hook {
        Ok(h) => h,
        Err(e) => {
            error!("Failed to install mouse hook: {}", e);
            HOOK_THREAD_ID.store(0, Ordering::SeqCst);
            HOOK_RUNNING.store(false, Ordering::SeqCst);
            return;
        }
    };

    info!("Mouse hook thread started (thread_id={})", thread_id);

    // Message pump — required for low-level hooks to work.
    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !ret.as_bool() {
            // WM_QUIT received or error
            break;
        }
    }

    // Unhook
    let _ = unsafe { UnhookWindowsHookEx(hook) };
    HOOK_THREAD_ID.store(0, Ordering::SeqCst);
    HOOK_RUNNING.store(false, Ordering::SeqCst);

    info!("Mouse hook thread stopped");
}

// ============================================================================
// Hook callback
// ============================================================================

/// Read current keyboard modifier state.
fn get_current_modifiers() -> u8 {
    let mut mods: u8 = modifiers::NONE;
    unsafe {
        if GetAsyncKeyState(VK_CONTROL.0 as i32) < 0 {
            mods |= modifiers::CTRL;
        }
        if GetAsyncKeyState(VK_SHIFT.0 as i32) < 0 {
            mods |= modifiers::SHIFT;
        }
        if GetAsyncKeyState(VK_MENU.0 as i32) < 0 {
            mods |= modifiers::ALT;
        }
        if GetAsyncKeyState(VK_LWIN.0 as i32) < 0 || GetAsyncKeyState(VK_RWIN.0 as i32) < 0 {
            mods |= modifiers::META;
        }
    }
    mods
}

const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;

/// Extract XBUTTON number from mouseData field (HIWORD).
fn xbutton_from_mousedata(mouse_data: u32) -> Option<MouseButton> {
    match mouse_data >> 16 {
        XBUTTON1 => Some(MouseButton::X1),
        XBUTTON2 => Some(MouseButton::X2),
        _ => None,
    }
}

/// Deliver an event to the app if the registry wants it. Returns true only if
/// the event was actually queued — the hook blocks the OS event only then, so
/// a disconnected app never silently swallows mouse buttons.
fn send_hook_event(event: &MouseEvent) -> bool {
    HOOK_REGISTRY
        .get()
        .and_then(|lock| lock.lock().ok())
        .and_then(|guard| {
            let registry = guard.as_ref()?;
            if registry.should_deliver(event) {
                Some(registry.send_event(*event))
            } else {
                Some(false)
            }
        })
        .unwrap_or(false)
}

/// The low-level mouse hook procedure.
unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };

        let (button, is_down) = match wparam.0 as u32 {
            x if x == WM_XBUTTONDOWN => (xbutton_from_mousedata(data.mouseData), true),
            x if x == WM_XBUTTONUP => (xbutton_from_mousedata(data.mouseData), false),
            x if x == WM_MBUTTONDOWN => (Some(MouseButton::Middle), true),
            x if x == WM_MBUTTONUP => (Some(MouseButton::Middle), false),
            _ => (None, false),
        };

        if let Some(btn) = button {
            let event = MouseEvent {
                button: btn,
                mods: get_current_modifiers(),
                is_down,
            };
            if send_hook_event(&event) {
                // Block: do not pass to other applications
                return LRESULT(1);
            }
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xbutton_extraction() {
        assert_eq!(
            xbutton_from_mousedata(XBUTTON1 << 16),
            Some(MouseButton::X1)
        );
        assert_eq!(
            xbutton_from_mousedata(XBUTTON2 << 16),
            Some(MouseButton::X2)
        );
        assert_eq!(xbutton_from_mousedata(0), None);
    }

    #[test]
    fn hook_starts_lazily_with_registry() {
        // No bindings → no hook thread is spawned.
        let registry = MouseRegistry::new();
        let state = MouseHookState::new(&registry);
        assert_eq!(HOOK_THREAD_ID.load(Ordering::SeqCst), 0);

        // Registering a binding activates the registry and installs the hook.
        registry
            .register("lazy_hook_test", MouseButton::X1, modifiers::NONE, "mouse4")
            .unwrap();
        for _ in 0..200 {
            if HOOK_THREAD_ID.load(Ordering::SeqCst) != 0 {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_ne!(HOOK_THREAD_ID.load(Ordering::SeqCst), 0);

        // Unregistering everything tears the hook down again.
        registry.unregister("lazy_hook_test");
        for _ in 0..200 {
            if HOOK_THREAD_ID.load(Ordering::SeqCst) == 0 {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(HOOK_THREAD_ID.load(Ordering::SeqCst), 0);

        drop(state);
    }
}
