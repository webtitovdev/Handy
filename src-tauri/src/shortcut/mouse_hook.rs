//! Low-level mouse hook for Windows.
//!
//! Captures mouse button events (X1/X2/Middle) system-wide using
//! `SetWindowsHookExW(WH_MOUSE_LL, ...)`. Matched events are blocked
//! from reaching other applications.
//!
//! ## Architecture
//!
//! ```text
//! ┌──────────────────┐  start/stop   ┌──────────────────────┐
//! │   Main Thread     │ ────────────▶ │  Mouse Hook Thread   │
//! │                   │               │                      │
//! │ - start()        │   events      │ - SetWindowsHookExW  │
//! │ - stop()         │ ◀──────────── │ - WH_MOUSE_LL        │
//! │ - register()     │  (channel)    │ - GetMessage loop    │
//! │ - unregister()   │               └──────────────────────┘
//! └──────────────────┘
//! ```

use log::{debug, error, info};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_MENU, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    MSLLHOOKSTRUCT, MSG, WH_MOUSE_LL, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_QUIT,
    WM_XBUTTONDOWN, WM_XBUTTONUP,
};

// ============================================================================
// Types
// ============================================================================

/// Supported mouse buttons for shortcut bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    X1,     // mouse4 / back
    X2,     // mouse5 / forward
    Middle, // middle click (scroll wheel press)
}

impl MouseButton {
    /// Parse a mouse button from a key name string.
    pub fn from_key_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "mouse4" | "mousex1" | "xbutton1" | "back" => Some(MouseButton::X1),
            "mouse5" | "mousex2" | "xbutton2" | "forward" => Some(MouseButton::X2),
            "mousemiddle" | "middleclick" => Some(MouseButton::Middle),
            _ => None,
        }
    }

    /// Canonical string representation used in binding strings.
    pub fn to_key_name(&self) -> &'static str {
        match self {
            MouseButton::X1 => "mouse4",
            MouseButton::X2 => "mouse5",
            MouseButton::Middle => "mousemiddle",
        }
    }
}

/// Modifier bitmask flags.
pub mod modifiers {
    pub const CTRL: u8 = 1;
    pub const SHIFT: u8 = 2;
    pub const ALT: u8 = 4;
}

/// A mouse event from the hook.
#[derive(Debug, Clone)]
pub struct MouseHookEvent {
    pub button: MouseButton,
    pub mods: u8,
    pub is_down: bool,
}

/// A registered mouse button binding.
#[derive(Debug, Clone)]
struct MouseBinding {
    binding_id: String,
    button: MouseButton,
    mods: u8,
    hotkey_string: String,
}

// ============================================================================
// Global state for hook callback
// ============================================================================

/// Channel sender for hook callback → event processing.
/// Must be global because `SetWindowsHookExW` callback is a C function pointer.
static HOOK_EVENT_SENDER: OnceLock<Mutex<Option<Sender<MouseHookEvent>>>> = OnceLock::new();

/// Whether we are currently in recording mode (hook should capture all supported buttons).
static RECORDING_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Registered bindings for fast lookup from the hook callback.
/// Must be global because the hook callback is a C function pointer without user data.
static REGISTERED_BINDINGS: OnceLock<Mutex<Vec<(MouseButton, u8)>>> = OnceLock::new();

/// Thread ID for the hook thread (used for PostThreadMessage to stop it).
static HOOK_THREAD_ID: AtomicU32 = AtomicU32::new(0);

// ============================================================================
// MouseHookState — public API
// ============================================================================

/// State for the mouse hook shortcut manager.
pub struct MouseHookState {
    thread_handle: Mutex<Option<JoinHandle<()>>>,
    is_active: AtomicBool,
    /// Channel to receive mouse events from the hook callback.
    event_receiver: Mutex<Receiver<MouseHookEvent>>,
    /// Registered bindings (binding_id → MouseBinding).
    bindings: Mutex<Vec<MouseBinding>>,
}

impl MouseHookState {
    /// Create and start the mouse hook.
    pub fn new() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<MouseHookEvent>();

        // Initialize global sender
        let sender_lock = HOOK_EVENT_SENDER.get_or_init(|| Mutex::new(None));
        if let Ok(mut guard) = sender_lock.lock() {
            *guard = Some(tx);
        }

        // Initialize or clear registered bindings (in case of re-creation after stop)
        let bindings_lock = REGISTERED_BINDINGS.get_or_init(|| Mutex::new(Vec::new()));
        if let Ok(mut guard) = bindings_lock.lock() {
            guard.clear();
        }

        // Start hook thread
        let handle = thread::spawn(|| {
            hook_thread_main();
        });

        Ok(Self {
            thread_handle: Mutex::new(Some(handle)),
            is_active: AtomicBool::new(true),
            event_receiver: Mutex::new(rx),
            bindings: Mutex::new(Vec::new()),
        })
    }

    /// Try to receive a mouse event (non-blocking).
    pub fn try_recv(&self) -> Option<MouseHookEvent> {
        self.event_receiver
            .lock()
            .ok()
            .and_then(|rx| rx.try_recv().ok())
    }

    /// Check if a mouse event matches a registered binding and return the binding info.
    pub fn match_event(&self, event: &MouseHookEvent) -> Option<(String, String)> {
        let bindings = self.bindings.lock().ok()?;
        for b in bindings.iter() {
            if b.button == event.button && b.mods == event.mods {
                return Some((b.binding_id.clone(), b.hotkey_string.clone()));
            }
        }
        None
    }

    /// Register a mouse button binding.
    pub fn register(
        &self,
        binding_id: &str,
        button: MouseButton,
        mods: u8,
        hotkey_string: &str,
    ) -> Result<(), String> {
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|_| "Failed to lock mouse bindings")?;

        // Remove existing binding with same ID
        bindings.retain(|b| b.binding_id != binding_id);

        bindings.push(MouseBinding {
            binding_id: binding_id.to_string(),
            button,
            mods,
            hotkey_string: hotkey_string.to_string(),
        });

        // Update global registered bindings for the hook callback
        Self::sync_global_bindings(&bindings);

        debug!(
            "Registered mouse binding: {} -> {:?} mods={}",
            binding_id, button, mods
        );
        Ok(())
    }

    /// Unregister a mouse button binding.
    pub fn unregister(&self, binding_id: &str) -> Result<(), String> {
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|_| "Failed to lock mouse bindings")?;

        bindings.retain(|b| b.binding_id != binding_id);
        Self::sync_global_bindings(&bindings);

        debug!("Unregistered mouse binding: {}", binding_id);
        Ok(())
    }

    /// Enter recording mode — the hook will capture all supported button events.
    pub fn start_recording(&self) {
        RECORDING_ACTIVE.store(true, Ordering::SeqCst);
        debug!("Mouse hook entered recording mode");
    }

    /// Exit recording mode.
    pub fn stop_recording(&self) {
        RECORDING_ACTIVE.store(false, Ordering::SeqCst);
        debug!("Mouse hook exited recording mode");
    }

    /// Stop the hook and clean up.
    pub fn stop(&self) {
        if !self.is_active.swap(false, Ordering::SeqCst) {
            return; // Already stopped
        }

        // Post WM_QUIT to the hook thread to exit its message loop
        let thread_id = HOOK_THREAD_ID.load(Ordering::SeqCst);
        if thread_id != 0 {
            unsafe {
                let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }

        // Wait for thread to finish
        if let Ok(mut handle) = self.thread_handle.lock() {
            if let Some(h) = handle.take() {
                let _ = h.join();
            }
        }

        // Clear global sender
        if let Some(sender_lock) = HOOK_EVENT_SENDER.get() {
            if let Ok(mut guard) = sender_lock.lock() {
                *guard = None;
            }
        }

        info!("Mouse hook stopped");
    }

    /// Sync local bindings to global static for hook callback access.
    fn sync_global_bindings(bindings: &[MouseBinding]) {
        if let Some(global) = REGISTERED_BINDINGS.get() {
            if let Ok(mut guard) = global.lock() {
                *guard = bindings.iter().map(|b| (b.button, b.mods)).collect();
            }
        }
    }
}

impl Drop for MouseHookState {
    fn drop(&mut self) {
        self.stop();
    }
}

// ============================================================================
// Hook thread
// ============================================================================

/// Main function for the hook thread.
fn hook_thread_main() {
    // Store thread ID so we can post WM_QUIT later
    let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    HOOK_THREAD_ID.store(thread_id, Ordering::SeqCst);

    info!("Mouse hook thread started (thread_id={})", thread_id);

    // Install the low-level mouse hook
    let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) };

    let hook = match hook {
        Ok(h) => h,
        Err(e) => {
            error!("Failed to install mouse hook: {}", e);
            return;
        }
    };

    // Message pump — required for low-level hooks to work
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

    info!("Mouse hook thread stopped");
}

// ============================================================================
// Hook callback
// ============================================================================

/// Read current keyboard modifier state.
fn get_current_modifiers() -> u8 {
    let mut mods: u8 = 0;
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

/// Check if a (button, mods) pair matches any registered binding.
fn is_registered(button: MouseButton, mods: u8) -> bool {
    if let Some(global) = REGISTERED_BINDINGS.get() {
        if let Ok(guard) = global.lock() {
            return guard.iter().any(|(b, m)| *b == button && *m == mods);
        }
    }
    false
}

/// Send event through the global channel. Returns true only if the event
/// was actually delivered — the hook should block the OS event only then.
fn send_hook_event(button: MouseButton, mods: u8, is_down: bool) -> bool {
    let is_recording = RECORDING_ACTIVE.load(Ordering::SeqCst);
    let is_matched = is_registered(button, mods);

    if !is_recording && !is_matched {
        return false;
    }

    // Only block the OS event if we successfully deliver it to the app.
    // Otherwise let it pass through so the button isn't silently swallowed.
    let delivered = HOOK_EVENT_SENDER
        .get()
        .and_then(|lock| lock.lock().ok())
        .and_then(|guard| {
            guard.as_ref().map(|tx| {
                tx.send(MouseHookEvent {
                    button,
                    mods,
                    is_down,
                })
                .is_ok()
            })
        })
        .unwrap_or(false);

    delivered
}

/// The low-level mouse hook procedure.
unsafe extern "system" fn mouse_hook_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
            let mods = get_current_modifiers();
            if send_hook_event(btn, mods, is_down) {
                // Block: do not pass to other applications
                return LRESULT(1);
            }
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

// ============================================================================
// Parsing helpers
// ============================================================================

/// Check whether a hotkey string refers to a mouse button binding.
pub fn is_mouse_binding(hotkey_str: &str) -> bool {
    hotkey_str
        .split('+')
        .any(|part| MouseButton::from_key_name(part.trim()).is_some())
}

/// Parse a mouse binding string like "ctrl+mouse4" into (MouseButton, modifier bitmask).
pub fn parse_mouse_binding(hotkey_str: &str) -> Result<(MouseButton, u8), String> {
    let parts: Vec<&str> = hotkey_str.split('+').collect();
    let mut mods: u8 = 0;
    let mut button: Option<MouseButton> = None;

    for part in &parts {
        let trimmed = part.trim().to_lowercase();
        match trimmed.as_str() {
            "ctrl" | "control" => mods |= modifiers::CTRL,
            "shift" => mods |= modifiers::SHIFT,
            "alt" | "option" => mods |= modifiers::ALT,
            _ => {
                if let Some(btn) = MouseButton::from_key_name(&trimmed) {
                    if button.is_some() {
                        return Err(format!(
                            "Multiple mouse buttons in binding: '{}'",
                            hotkey_str
                        ));
                    }
                    button = Some(btn);
                } else {
                    return Err(format!(
                        "Unknown key in mouse binding: '{}' (full: '{}')",
                        trimmed, hotkey_str
                    ));
                }
            }
        }
    }

    match button {
        Some(btn) => Ok((btn, mods)),
        None => Err(format!("No mouse button found in: '{}'", hotkey_str)),
    }
}

/// Build modifier name list for FrontendKeyEvent (matches handy-keys naming).
pub fn modifiers_to_strings(mods: u8) -> Vec<String> {
    let mut result = Vec::new();
    if mods & modifiers::CTRL != 0 {
        result.push("ctrl".to_string());
    }
    if mods & modifiers::SHIFT != 0 {
        result.push("shift".to_string());
    }
    if mods & modifiers::ALT != 0 {
        result.push("alt".to_string());
    }
    result
}

/// Format a mouse binding back to a canonical hotkey string (e.g. "ctrl+mouse4").
pub fn format_mouse_hotkey(button: MouseButton, mods: u8) -> String {
    let mut parts = modifiers_to_strings(mods);
    parts.push(button.to_key_name().to_string());
    parts.join("+")
}

/// Validate a mouse binding string.
pub fn validate_mouse_binding(raw: &str) -> Result<(), String> {
    parse_mouse_binding(raw).map(|_| ())
}
