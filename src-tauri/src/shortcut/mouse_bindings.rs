//! Platform-independent mouse button shortcut support.
//!
//! This module owns everything that does not depend on an OS-specific event
//! source: the button vocabulary, binding-string parsing/formatting,
//! validation, and the registry of active bindings plus the event queue the
//! capture backend feeds.
//!
//! The actual capture is delegated to a platform backend:
//!
//! | Platform | Backend                      | Status      |
//! |----------|------------------------------|-------------|
//! | Windows  | `mouse_hook` (`WH_MOUSE_LL`) | supported   |
//! | macOS    | CGEventTap (planned)         | unsupported |
//! | Linux    | evdev / X11 (planned)        | unsupported |
//!
//! A backend only has to (1) forward captured transitions with
//! [`MouseRegistry::send_event`] after asking [`MouseRegistry::should_deliver`],
//! and (2) attach itself with [`MouseRegistry::on_active_change`] so it is
//! started/stopped on demand. `mouse_hook.rs` is the reference implementation.

use log::{debug, warn};
use serde::Serialize;
use specta::Type;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};

/// Whether a platform backend exists that can capture mouse buttons on this OS.
///
/// When `false`, mouse bindings are rejected at validation time so an unusable
/// shortcut can never be persisted to settings.
pub const MOUSE_CAPTURE_SUPPORTED: bool = cfg!(target_os = "windows");

/// Reason shown when mouse bindings are unavailable on this platform.
pub const MOUSE_UNSUPPORTED_MESSAGE: &str =
    "Mouse button shortcuts are not supported on this platform yet";

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
    /// All bindable buttons, in canonical order.
    #[allow(dead_code)] // used by tests and future backends (macOS/Linux)
    pub const ALL: [MouseButton; 3] = [MouseButton::X1, MouseButton::X2, MouseButton::Middle];

    /// Parse a mouse button from a key name string.
    pub fn from_key_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "mouse4" | "mousex1" | "xbutton1" | "back" => Some(MouseButton::X1),
            "mouse5" | "mousex2" | "xbutton2" | "forward" => Some(MouseButton::X2),
            "mousemiddle" | "middleclick" | "middle" => Some(MouseButton::Middle),
            _ => None,
        }
    }

    /// Canonical string representation used in binding strings.
    pub fn to_key_name(self) -> &'static str {
        match self {
            MouseButton::X1 => "mouse4",
            MouseButton::X2 => "mouse5",
            MouseButton::Middle => "mousemiddle",
        }
    }
}

/// Modifier bitmask flags used by mouse bindings.
pub mod modifiers {
    pub const NONE: u8 = 0;
    pub const CTRL: u8 = 1;
    pub const SHIFT: u8 = 2;
    pub const ALT: u8 = 4;
    /// Command (macOS) / Super (Windows, Linux).
    pub const META: u8 = 8;
}

/// A mouse button transition reported by a platform backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub button: MouseButton,
    pub mods: u8,
    pub is_down: bool,
}

/// Serializable mouse event payload for the frontend while recording.
#[derive(Debug, Clone, Serialize, Type)]
pub struct FrontendMouseEvent {
    pub modifiers: Vec<String>,
    pub key: String,
    pub is_key_down: bool,
    pub hotkey_string: String,
}

impl FrontendMouseEvent {
    /// Build the frontend payload for a raw backend event.
    pub fn from_event(event: &MouseEvent) -> Self {
        Self {
            modifiers: modifiers_to_strings(event.mods),
            key: event.button.to_key_name().to_string(),
            is_key_down: event.is_down,
            hotkey_string: format_mouse_hotkey(event.button, event.mods),
        }
    }
}

/// A registered mouse button binding.
#[derive(Debug, Clone)]
struct MouseBinding {
    binding_id: String,
    button: MouseButton,
    mods: u8,
    hotkey_string: String,
}

/// Callback notified when a capture backend should start (`true`) or stop.
type ActiveListener = Arc<dyn Fn(bool) + Send + Sync>;

// ============================================================================
// Registry
// ============================================================================

/// Shared registry of mouse bindings and the queue of captured events.
///
/// Cloning yields another handle to the same registry, so the manager thread,
/// the recording loop and the platform backend can all share one instance
/// without going through Tauri's managed state — which is not populated yet
/// while shortcuts are being registered during startup.
#[derive(Clone, Default)]
pub struct MouseRegistry {
    inner: Arc<RegistryInner>,
}

impl std::fmt::Debug for MouseRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MouseRegistry")
            .field("bindings", &self.binding_count())
            .field("recording", &self.is_recording())
            .finish()
    }
}

struct RegistryInner {
    bindings: Mutex<Vec<MouseBinding>>,
    /// True while the UI is capturing a new shortcut: every supported button
    /// is reported (and swallowed) so it can be recorded.
    recording: AtomicBool,
    /// Buttons whose *down* event was delivered; used to drop stray releases.
    pressed: Mutex<HashSet<MouseButton>>,
    /// Notified whenever the registry switches between needing a capture
    /// backend and not needing one.
    active_listeners: Mutex<Vec<ActiveListener>>,
    /// Event queue: the backend produces, the manager thread (or the
    /// recording loop while recording) consumes.
    event_tx: Sender<MouseEvent>,
    event_rx: Mutex<Receiver<MouseEvent>>,
}

impl Default for RegistryInner {
    fn default() -> Self {
        let (event_tx, event_rx) = mpsc::channel::<MouseEvent>();
        Self {
            bindings: Mutex::new(Vec::new()),
            recording: AtomicBool::new(false),
            pressed: Mutex::new(HashSet::new()),
            active_listeners: Mutex::new(Vec::new()),
            event_tx,
            event_rx: Mutex::new(event_rx),
        }
    }
}

impl MouseRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a mouse button binding, replacing any binding with the same id.
    ///
    /// Errors when a *different* binding already uses the same button +
    /// modifier combination, since the first match would shadow the second.
    pub fn register(
        &self,
        binding_id: &str,
        button: MouseButton,
        mods: u8,
        hotkey_string: &str,
    ) -> Result<(), String> {
        let mut bindings = self
            .inner
            .bindings
            .lock()
            .map_err(|_| "Failed to lock mouse bindings")?;

        if let Some(conflict) = bindings
            .iter()
            .find(|b| b.binding_id != binding_id && b.button == button && b.mods == mods)
        {
            return Err(format!(
                "'{}' is already used by the '{}' shortcut",
                format_mouse_hotkey(button, mods),
                conflict.binding_id
            ));
        }

        bindings.retain(|b| b.binding_id != binding_id);
        bindings.push(MouseBinding {
            binding_id: binding_id.to_string(),
            button,
            mods,
            hotkey_string: hotkey_string.to_string(),
        });

        let count = bindings.len();
        drop(bindings);

        debug!(
            "Registered mouse binding: {} -> {:?} mods={} ({} total)",
            binding_id, button, mods, count
        );

        self.notify_active();
        Ok(())
    }

    /// Unregister a mouse button binding. Unknown ids are ignored.
    pub fn unregister(&self, binding_id: &str) {
        let removed = {
            let Ok(mut bindings) = self.inner.bindings.lock() else {
                return;
            };
            let before = bindings.len();
            bindings.retain(|b| b.binding_id != binding_id);
            before != bindings.len()
        };

        if removed {
            debug!("Unregistered mouse binding: {}", binding_id);
            self.notify_active();
        }
    }

    /// Number of registered bindings.
    pub fn binding_count(&self) -> usize {
        self.inner
            .bindings
            .lock()
            .map(|b| b.len())
            .unwrap_or_default()
    }

    /// Forget every binding (shutdown, or re-initialization of the manager).
    pub fn clear(&self) {
        if let Ok(mut bindings) = self.inner.bindings.lock() {
            bindings.clear();
        }
        self.notify_active();
    }

    /// Find the binding matching an event, if any.
    pub fn match_event(&self, event: &MouseEvent) -> Option<(String, String)> {
        let bindings = self.inner.bindings.lock().ok()?;
        bindings
            .iter()
            .find(|b| b.button == event.button && b.mods == event.mods)
            .map(|b| (b.binding_id.clone(), b.hotkey_string.clone()))
    }

    /// True while the UI is recording a new shortcut.
    pub fn is_recording(&self) -> bool {
        self.inner.recording.load(Ordering::SeqCst)
    }

    /// Enter recording mode: every supported button is reported and blocked.
    pub fn start_recording(&self) {
        self.inner.recording.store(true, Ordering::SeqCst);
        debug!("Mouse registry entered recording mode");
        self.notify_active();
    }

    /// Leave recording mode.
    pub fn stop_recording(&self) {
        self.inner.recording.store(false, Ordering::SeqCst);
        // A button held while recording ended must not later produce a
        // release event for a binding that was never started.
        if let Ok(mut pressed) = self.inner.pressed.lock() {
            pressed.clear();
        }
        // Discard events captured while recording: they were reported to the
        // UI already, and leaving them queued would replay the just-recorded
        // button press against the freshly registered binding.
        self.drain_events();
        debug!("Mouse registry exited recording mode");
        self.notify_active();
    }

    /// Decide whether an event belongs to the app (and should therefore be
    /// swallowed instead of passed on to other applications).
    ///
    /// Releases are only accepted when the matching press was accepted too,
    /// which keeps push-to-talk from seeing a "stop" without a "start".
    pub fn should_deliver(&self, event: &MouseEvent) -> bool {
        if self.is_recording() {
            self.track_press(event);
            return true;
        }

        if self.match_event(event).is_none() {
            return false;
        }

        if !event.is_down && !self.was_pressed(event.button) {
            // Stray release (button already held when the app started, or the
            // press went to another app). Drop it instead of firing an action.
            debug!(
                "Dropping mouse release without matching press: {:?}",
                event.button
            );
            return false;
        }

        self.track_press(event);
        true
    }

    /// Backend → app: queue a captured event for the dispatcher.
    pub fn send_event(&self, event: MouseEvent) -> bool {
        self.inner.event_tx.send(event).is_ok()
    }

    /// App side: take the next captured event without blocking.
    pub fn try_recv_event(&self) -> Option<MouseEvent> {
        let rx = self.inner.event_rx.lock().ok()?;
        match rx.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Discard every queued event.
    pub fn drain_events(&self) {
        while self.try_recv_event().is_some() {}
    }

    /// Attach a callback fired whenever the backend should start or stop.
    ///
    /// The callback also runs immediately with the current state, so a backend
    /// attached after bindings were registered still starts up.
    pub fn on_active_change(&self, listener: ActiveListener) {
        let active = self.is_active();
        if let Ok(mut listeners) = self.inner.active_listeners.lock() {
            listeners.push(Arc::clone(&listener));
        }
        listener(active);
    }

    /// True when a capture backend is needed: something is bound or recording.
    pub fn is_active(&self) -> bool {
        self.is_recording() || self.binding_count() > 0
    }

    /// True when the press for this button was delivered earlier.
    pub fn was_pressed(&self, button: MouseButton) -> bool {
        self.inner
            .pressed
            .lock()
            .map(|p| p.contains(&button))
            .unwrap_or(false)
    }

    fn track_press(&self, event: &MouseEvent) {
        let Ok(mut pressed) = self.inner.pressed.lock() else {
            return;
        };
        if event.is_down {
            pressed.insert(event.button);
        } else {
            pressed.remove(&event.button);
        }
    }

    fn notify_active(&self) {
        let active = self.is_active();
        let Ok(listeners) = self.inner.active_listeners.lock() else {
            return;
        };
        for listener in listeners.iter() {
            listener(active);
        }
    }
}

// ============================================================================
// Parsing / formatting
// ============================================================================

/// Check whether a hotkey string refers to a mouse button binding.
pub fn is_mouse_binding(hotkey_str: &str) -> bool {
    hotkey_str
        .split('+')
        .any(|part| MouseButton::from_key_name(part.trim()).is_some())
}

/// Parse a mouse binding string like "ctrl+mouse4" into (button, modifiers).
pub fn parse_mouse_binding(hotkey_str: &str) -> Result<(MouseButton, u8), String> {
    let mut mods: u8 = modifiers::NONE;
    let mut button: Option<MouseButton> = None;

    for part in hotkey_str.split('+') {
        let trimmed = part.trim().to_lowercase();
        if trimmed.is_empty() {
            continue;
        }

        match trimmed.as_str() {
            "ctrl" | "control" => mods |= modifiers::CTRL,
            "shift" => mods |= modifiers::SHIFT,
            "alt" | "option" => mods |= modifiers::ALT,
            "cmd" | "command" | "super" | "meta" | "win" | "windows" => mods |= modifiers::META,
            _ => {
                let Some(parsed) = MouseButton::from_key_name(&trimmed) else {
                    return Err(format!(
                        "Unknown key in mouse binding: '{}' (full: '{}')",
                        trimmed, hotkey_str
                    ));
                };
                if button.is_some() {
                    return Err(format!(
                        "Multiple mouse buttons in binding: '{}'",
                        hotkey_str
                    ));
                }
                button = Some(parsed);
            }
        }
    }

    match button {
        Some(btn) => Ok((btn, mods)),
        None => Err(format!("No mouse button found in: '{}'", hotkey_str)),
    }
}

/// Build modifier name list for the frontend (matches handy-keys naming).
pub fn modifiers_to_strings(mods: u8) -> Vec<String> {
    let mut result = Vec::new();
    if mods & modifiers::CTRL != 0 {
        result.push("ctrl".to_string());
    }
    if mods & modifiers::SHIFT != 0 {
        result.push("shift".to_string());
    }
    if mods & modifiers::ALT != 0 {
        #[cfg(target_os = "macos")]
        result.push("option".to_string());
        #[cfg(not(target_os = "macos"))]
        result.push("alt".to_string());
    }
    if mods & modifiers::META != 0 {
        #[cfg(target_os = "macos")]
        result.push("command".to_string());
        #[cfg(not(target_os = "macos"))]
        result.push("super".to_string());
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

/// Validate a mouse binding for the current platform.
///
/// On platforms without a capture backend this always fails, which keeps
/// unsupported shortcuts out of persisted settings instead of saving a binding
/// that silently never fires.
pub fn validate_for_platform(raw: &str) -> Result<(), String> {
    if !MOUSE_CAPTURE_SUPPORTED {
        warn!(
            "Rejecting mouse binding '{}' — no capture backend on this platform",
            raw
        );
        return Err(MOUSE_UNSUPPORTED_MESSAGE.to_string());
    }
    validate_mouse_binding(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(button: MouseButton, mods: u8, is_down: bool) -> MouseEvent {
        MouseEvent {
            button,
            mods,
            is_down,
        }
    }

    #[test]
    fn parses_bare_button() {
        assert_eq!(
            parse_mouse_binding("mouse4").unwrap(),
            (MouseButton::X1, modifiers::NONE)
        );
        assert_eq!(
            parse_mouse_binding("MouseMiddle").unwrap(),
            (MouseButton::Middle, modifiers::NONE)
        );
    }

    #[test]
    fn parses_modifiers_in_any_order() {
        assert_eq!(
            parse_mouse_binding("ctrl+shift+mouse5").unwrap(),
            (MouseButton::X2, modifiers::CTRL | modifiers::SHIFT)
        );
        assert_eq!(
            parse_mouse_binding("mouse5+shift+ctrl").unwrap(),
            (MouseButton::X2, modifiers::CTRL | modifiers::SHIFT)
        );
    }

    #[test]
    fn accepts_aliases() {
        for alias in ["mouse4", "mousex1", "xbutton1", "back"] {
            assert_eq!(
                MouseButton::from_key_name(alias),
                Some(MouseButton::X1),
                "alias {} should map to X1",
                alias
            );
        }
    }

    #[test]
    fn rejects_unknown_and_multi_button() {
        assert!(parse_mouse_binding("ctrl+space").is_err());
        assert!(parse_mouse_binding("mouse4+mouse5").is_err());
        assert!(parse_mouse_binding("").is_err());
    }

    #[test]
    fn format_is_stable_round_trip() {
        for button in MouseButton::ALL {
            for mods in [
                modifiers::NONE,
                modifiers::CTRL,
                modifiers::CTRL | modifiers::SHIFT | modifiers::ALT,
            ] {
                let formatted = format_mouse_hotkey(button, mods);
                let (parsed_button, parsed_mods) = parse_mouse_binding(&formatted).unwrap();
                assert_eq!(parsed_button, button);
                assert_eq!(parsed_mods, mods);
            }
        }
    }

    #[test]
    fn detects_mouse_bindings() {
        assert!(is_mouse_binding("mouse4"));
        assert!(is_mouse_binding("ctrl+mouse5"));
        assert!(!is_mouse_binding("ctrl+space"));
    }

    #[test]
    fn registry_rejects_conflicting_combinations() {
        let registry = MouseRegistry::new();
        registry
            .register("transcribe", MouseButton::X1, modifiers::NONE, "mouse4")
            .unwrap();

        assert!(registry
            .register(
                "transcribe_with_post_process",
                MouseButton::X1,
                modifiers::NONE,
                "mouse4"
            )
            .is_err());

        // The same id may be re-registered (that is how a shortcut changes).
        registry
            .register(
                "transcribe",
                MouseButton::X1,
                modifiers::CTRL,
                "ctrl+mouse4",
            )
            .unwrap();
        assert_eq!(registry.binding_count(), 1);
    }

    #[test]
    fn registry_matches_button_and_modifiers_exactly() {
        let registry = MouseRegistry::new();
        registry
            .register(
                "transcribe",
                MouseButton::X1,
                modifiers::CTRL,
                "ctrl+mouse4",
            )
            .unwrap();

        assert!(registry
            .match_event(&event(MouseButton::X1, modifiers::CTRL, true))
            .is_some());
        assert!(registry
            .match_event(&event(MouseButton::X1, modifiers::NONE, true))
            .is_none());
    }

    #[test]
    fn registry_is_active_only_when_needed() {
        let registry = MouseRegistry::new();
        assert!(!registry.is_active());

        registry
            .register("transcribe", MouseButton::X1, modifiers::NONE, "mouse4")
            .unwrap();
        assert!(registry.is_active());

        registry.unregister("transcribe");
        assert!(!registry.is_active());

        registry.start_recording();
        assert!(registry.is_active());
        registry.stop_recording();
        assert!(!registry.is_active());
    }

    #[test]
    fn release_without_press_is_dropped() {
        let registry = MouseRegistry::new();
        registry
            .register("transcribe", MouseButton::X1, modifiers::NONE, "mouse4")
            .unwrap();

        assert!(registry.should_deliver(&event(MouseButton::X1, 0, true)));
        assert!(registry.was_pressed(MouseButton::X1));
        assert!(registry.should_deliver(&event(MouseButton::X1, 0, false)));
        assert!(!registry.was_pressed(MouseButton::X1));

        // A second release with no press in between must not fire.
        assert!(!registry.should_deliver(&event(MouseButton::X1, 0, false)));
    }

    #[test]
    fn recording_mode_reports_everything() {
        let registry = MouseRegistry::new();
        registry.start_recording();
        assert!(registry.should_deliver(&event(MouseButton::Middle, 0, true)));

        registry.stop_recording();
        assert!(!registry.should_deliver(&event(MouseButton::Middle, 0, true)));
    }

    #[test]
    fn events_round_trip_through_the_queue() {
        let registry = MouseRegistry::new();
        let clone = registry.clone();

        assert!(registry.send_event(event(MouseButton::X2, 0, true)));
        assert_eq!(
            clone.try_recv_event(),
            Some(event(MouseButton::X2, 0, true))
        );
        assert_eq!(clone.try_recv_event(), None);
    }

    #[test]
    fn listener_sees_current_state_on_attach() {
        let registry = MouseRegistry::new();
        registry
            .register("transcribe", MouseButton::X1, modifiers::NONE, "mouse4")
            .unwrap();

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        registry.on_active_change(Arc::new(move |active: bool| {
            sink.lock().unwrap().push(active);
        }));

        assert_eq!(*seen.lock().unwrap(), vec![true]);
    }

    #[test]
    fn platform_validation_matches_support_flag() {
        if MOUSE_CAPTURE_SUPPORTED {
            assert!(validate_for_platform("mouse4").is_ok());
        } else {
            assert_eq!(
                validate_for_platform("mouse4").unwrap_err(),
                MOUSE_UNSUPPORTED_MESSAGE
            );
        }
    }
}
