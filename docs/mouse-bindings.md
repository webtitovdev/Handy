# Mouse Button Bindings — Architecture & Porting Guide

Mouse button shortcuts (Mouse 4 / Mouse 5 / Middle Click, optionally combined
with Ctrl/Shift/Alt/Super) are implemented in two layers:

```text
┌─────────────────────────────────────────────────────────────┐
│  mouse_bindings.rs  (platform-independent)                  │
│                                                             │
│  - MouseButton vocabulary + key-name parsing ("mouse4"…)    │
│  - parse/format/validate binding strings                    │
│  - MouseRegistry: bindings, recording mode, event queue,    │
│    press/release pairing, lazy-start notifications          │
└─────────────────────────────────────────────────────────────┘
              ▲ report events            │ on_active_change
              │ send_event()             ▼
┌─────────────────────────────────────────────────────────────┐
│  Platform capture backend (one per OS)                      │
│                                                             │
│  - Windows: mouse_hook.rs (WH_MOUSE_LL)      ✅ shipped     │
│  - macOS:   CGEventTap                       ⬜ planned     │
│  - Linux:   evdev / X11                      ⬜ planned     │
└─────────────────────────────────────────────────────────────┘
```

## Why this split exists

Mouse bindings used to be registered through
`app.try_state::<HandyKeysState>()`. During startup, `init_shortcuts()`
registers saved bindings **before** the state is handed to `app.manage()`,
so the lookup returned `None` and every persisted mouse binding silently
failed to register — the classic "I have to re-set my mouse button after
every restart" bug.

Now the shared `MouseRegistry` is created first and cloned into:

1. the handy-keys manager thread (registration + event dispatch),
2. the recording loop (UI capture),
3. the platform backend (event source).

No Tauri managed-state lookup is involved in any of those paths.

## Registry behaviour worth knowing

- **Lazy capture**: the backend only runs while the registry is *active*
  (≥1 binding registered or recording in progress). On Windows the
  `WH_MOUSE_LL` hook thread starts/stops accordingly, and a failed install
  is retried on the next activation.
- **Press/release pairing**: a button *release* is only delivered if the
  matching *press* was delivered first. This prevents a stray release
  (button already held when the app started) from stopping a
  push-to-talk recording that was never started.
- **Conflict detection**: registering the same button+modifier combo for
  two different shortcut ids is rejected (first match would shadow the
  second), and `change_binding` surfaces the error without unbinding the
  previous shortcut.
- **Recording drain**: events captured during UI recording are dropped
  when recording stops, so the just-recorded click does not immediately
  trigger the new binding.

## Platform gating

`mouse_bindings::MOUSE_CAPTURE_SUPPORTED` is `true` only where a backend
exists (`cfg!(target_os = "windows")` today). Validation rejects mouse
bindings on unsupported platforms **before** they can be persisted, so
settings never contain a shortcut that cannot fire. Flip the constant when
a backend lands.

The Tauri global-shortcut implementation never supports mouse buttons;
HandyKeys (the default on Windows/macOS) is required.

## Adding the macOS backend (planned)

The Windows backend (`mouse_hook.rs`, ~380 lines) is the reference. A macOS
port needs:

1. **Event tap** — `CGEvent::tap_create` (CoreGraphics) with
   `kCGEventMaskForAllEvents` filtered to:
   - `kCGEventOtherMouseDown` / `kCGEventOtherMouseUp` (button numbers 3
     and 4 → `MouseButton::X1` / `X2`; use
     `kCGMouseEventButtonNumber`),
   - `kCGEventMiddleMouseDown` / `kCGEventMiddleMouseUp` →
     `MouseButton::Middle`.
2. **Modifiers** — read `CGEventFlags` and map
   `kCGEventFlagMaskControl/Shift/Alternate/Command` to the bitmask in
   `mouse_bindings::modifiers` (META = Command on macOS).
3. **Blocking** — return `None` from the tap callback for delivered events
   (the macOS equivalent of returning `LRESULT(1)` on Windows).
4. **Run loop** — add the tap to a `CFRunLoop` on a dedicated thread;
   reuse the registry's `on_active_change` for lazy start/stop (enable/
   disable the tap instead of spawning/joining a thread if preferred).
5. **Permissions** — an event tap that blocks events requires
   Accessibility permission; a listen-only tap requires Input Monitoring.
   Handy already requests Accessibility for typing simulation, so hook
   into the existing onboarding/permission flow
   (`should_force_show_permissions_window`).
6. **Wiring** — in `handy_keys.rs`, replace the `#[cfg(target_os = "windows")]`
   backend construction with a `#[cfg(target_os = "macos")]` branch
   constructing the new backend against the same registry. Everything
   upstream (registration, dispatch, recording, frontend events,
   validation) is already platform-independent.
7. **Gate** — extend `MOUSE_CAPTURE_SUPPORTED` to
   `cfg!(any(target_os = "windows", target_os = "macos"))`.

Cargo dependency sketch (macOS only):

```toml
[target.'cfg(target_os = "macos")'.dependencies]
core-graphics = "0.25"   # already in the lockfile via tauri deps
core-foundation = "0.10"
```

Frontend needs no changes: `src/lib/utils/keyboard.ts` already renders
`mouse4` / `mouse5` / `mousemiddle` display names, and the recording UI is
backend-agnostic (it consumes `handy-keys-event` payloads).

Linux would follow the same pattern with evdev (`/dev/input/event*` +
`EVIOCGRAB` for exclusive blocking) or X11 `XGrabButton`; Wayland makes
global capture impractical, matching Handy's existing "limited Wayland
support" stance.
