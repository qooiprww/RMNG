//! macOS physical-keyboard capture via a raw `NSEvent` local monitor.
//!
//! **Why bypass GTK's `EventControllerKey` on macOS.** GDK's macOS backend does *not*
//! map raw scancodes to key events like the X11/Wayland backends do. `keyDown:` calls
//! `interpretKeyEvents:`, routing every key through the Cocoa Text Input Context (the
//! IME / marked-text machinery). For text the input method commits *outside* the
//! synchronous `keyDown:` — dead keys, marked text, press-and-hold accent popovers, real
//! IMEs — GDK emits a **synthetic "null key"** `GdkKeyEvent` with `hardware_keycode == 0`
//! and `keyval == VoidSymbol` (`gdk/macos: _gdk_macos_surface_synthesize_null_key`). Our
//! Carbon-kVK→evdev table maps keycode `0` to `KEY_A` (A is the one letter whose kVK is
//! literally 0), so those synthetic markers were forwarded to the remote as phantom,
//! out-of-order `A` presses — the "type a, nothing; type u, get `ua`" bug.
//!
//! A remote-desktop / game viewer wants **physical** keys, not IME-composed text (the
//! remote runs its own keymap). So on macOS we read raw key events straight from
//! `NSEvent` — `keyCode` is the true Carbon virtual key, delivered immediately, in order,
//! never synthesized — exactly as `pointer_lock_macos.rs` reads raw mouse deltas.
//!
//! **Integration.** One app-global local monitor (there is a single remote keyboard).
//! While a monitor/video window is the key window (see [`note_window_active`]) the monitor
//! forwards physical presses/releases and **consumes** them (`return null`) so AppKit never
//! runs `interpretKeyEvents:` on them (no beep, no null-key synthesis). Modifier transitions
//! (`FlagsChanged`) are forwarded but **passed through** so GDK/GTK keep their own modifier
//! state — the local shortcuts (F11, Ctrl+Alt+G/P) are still recognised by the GTK key
//! handler in `main.rs`, which is why those keys are passed through here too. When a dialog /
//! the pre-connection window is key instead, every event is passed through untouched so GTK
//! text entries and buttons work normally.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::Write;
use std::net::TcpStream;
use std::ptr::{null_mut, NonNull};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};

use crate::kvk_evdev;

/// The viewer's input write half (port-1 socket); shared with the GTK thread.
/// Same alias as `main.rs`'s `Writer` / `pointer_lock_macos.rs`.
type Writer = Arc<Mutex<Option<TcpStream>>>;

// Carbon kVK codes of the keys the GTK handler owns as *local* shortcuts (see
// `install_keyboard` in main.rs). The monitor passes these through instead of forwarding
// them, so the remote never sees them — matching Linux, where F11 / Ctrl+Alt+G /
// Ctrl+Alt+P are never put on the wire.
const KVK_F11: u32 = 0x67;
const KVK_G: u32 = 0x05;
const KVK_P: u32 = 0x23;

/// evdev `KEY_CAPSLOCK`. macOS reports CapsLock as a lock *toggle* (`FlagsChanged`), not a
/// hold, so it can't be tracked in the held-set like the other modifiers.
const KEY_CAPSLOCK: u32 = 58;

/// App-global state, initialised once by [`install`]. `active_windows` counts how many
/// video/monitor windows are currently the key window (0 ⇒ a dialog / the pre-connection
/// window has focus, so keys stay local); `pressed` is the set of evdev keycodes currently
/// held on the remote (so a focus loss / panic can release them); `_monitor` keeps the
/// NSEvent monitor installed for the app lifetime.
struct Shared {
    active_windows: Arc<AtomicUsize>,
    pressed: Arc<Mutex<HashSet<u32>>>,
    writer: Writer,
    _monitor: Retained<AnyObject>,
}

thread_local! {
    // The GTK application is single-threaded; the NSEvent monitor block runs on that same
    // main thread. A thread-local avoids threading this singleton through every window's
    // `install_keyboard` call.
    static KB: RefCell<Option<Shared>> = const { RefCell::new(None) };
}

/// Frame one input message to the server: `[0u8][u32be len][json]` (tag 0 = input).
/// One contiguous write, mirroring `send_tagged` in main.rs (TCP_NODELAY-friendly).
fn send_key(writer: &Writer, keycode: u32, pressed: bool) {
    let json = format!(r#"{{"kind":"key_code","keycode":{keycode},"pressed":{pressed}}}"#);
    let body = json.as_bytes();
    let mut frame = Vec::with_capacity(1 + 4 + body.len());
    frame.push(0u8);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(body);
    if let Some(g) = writer.lock().unwrap().as_mut() {
        let _ = g.write_all(&frame);
    }
}

/// Install the app-global keyboard monitor. Call once, from `build_ui`, on the main thread.
pub fn install(writer: Writer) {
    let active_windows = Arc::new(AtomicUsize::new(0));
    let pressed: Arc<Mutex<HashSet<u32>>> = Arc::new(Mutex::new(HashSet::new()));

    let block = {
        let (active_windows, pressed, writer) = (active_windows.clone(), pressed.clone(), writer.clone());
        // The handler returns the event to let it continue down the responder chain, or
        // `null` to consume it. (Same convention as the pointer-lock monitor.)
        RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: the ObjC runtime passes a valid, non-null NSEvent for the masked types.
            let ev = unsafe { event.as_ref() };

            // Not looking at the remote (a dialog or the pre-connection window is key):
            // let AppKit + GTK handle the key so local text entries / buttons work.
            if active_windows.load(Ordering::Relaxed) == 0 {
                return event.as_ptr();
            }

            let etype = ev.r#type().0;
            let kc = ev.keyCode() as u32;
            let mf = ev.modifierFlags().0;
            let ctrl = mf & NSEventModifierFlags::Control.0 != 0;
            let opt = mf & NSEventModifierFlags::Option.0 != 0;
            let is_local_shortcut = kc == KVK_F11 || (ctrl && opt && (kc == KVK_G || kc == KVK_P));

            if etype == NSEventType::KeyDown.0 {
                // Local viewer shortcuts stay with the GTK handler: pass them through (with
                // modifiers intact) and don't forward them to the remote.
                if is_local_shortcut {
                    return event.as_ptr();
                }
                // A held key already autorepeats on the remote (Mutter re-injects the held
                // keycode); don't stack a second local repeat stream on top.
                if ev.isARepeat() {
                    return null_mut();
                }
                let keycode = kvk_evdev::translate(kc);
                tracing::debug!("key down: kVK={:#04x} evdev={}", kc, keycode);
                if keycode != 0 {
                    pressed.lock().unwrap().insert(keycode);
                    send_key(&writer, keycode, true);
                }
                // Consume: no macOS beep, and — critically — no `interpretKeyEvents:`, so
                // GDK never synthesizes the phantom keycode-0 null-key this module fixes.
                null_mut()
            } else if etype == NSEventType::KeyUp.0 {
                // Only release keys we actually forwarded a press for. This naturally passes
                // through shortcut keys (never in the held-set) and avoids phantom releases.
                let keycode = kvk_evdev::translate(kc);
                tracing::debug!("key up: kVK={:#04x} evdev={}", kc, keycode);
                if keycode != 0 && pressed.lock().unwrap().remove(&keycode) {
                    send_key(&writer, keycode, false);
                    return null_mut();
                }
                event.as_ptr()
            } else if etype == NSEventType::FlagsChanged.0 {
                // Modifier transition. Forward it, but PASS IT THROUGH (don't consume) so
                // GDK/GTK keep their modifier state in sync — the F11 / Ctrl+Alt+G/P
                // shortcuts are recognised by the GTK handler from that state.
                let keycode = kvk_evdev::translate(kc);
                if keycode == KEY_CAPSLOCK {
                    // Lock key: AppKit reports a state toggle, not a hold. Emit a tap so the
                    // remote toggles its own lock. Best-effort (verify on-device).
                    send_key(&writer, keycode, true);
                    send_key(&writer, keycode, false);
                } else if keycode != 0 {
                    // Derive press vs release from our own held-set (robust to the left/right
                    // device-flag ambiguity in `modifierFlags`).
                    let mut held = pressed.lock().unwrap();
                    if held.remove(&keycode) {
                        drop(held);
                        send_key(&writer, keycode, false);
                    } else {
                        held.insert(keycode);
                        drop(held);
                        send_key(&writer, keycode, true);
                    }
                }
                event.as_ptr()
            } else {
                event.as_ptr()
            }
        })
    };

    let mask = NSEventMask::KeyDown | NSEventMask::KeyUp | NSEventMask::FlagsChanged;

    // SAFETY: called on the main thread; block is heap-allocated via RcBlock.
    let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) };
    let Some(monitor) = monitor else {
        tracing::warn!(
            "macOS keyboard: NSEvent local monitor install failed (runtime returned nil); \
             keyboard input will NOT reach the remote"
        );
        return;
    };

    KB.with(|k| *k.borrow_mut() = Some(Shared { active_windows, pressed, writer, _monitor: monitor }));
    tracing::info!("macOS keyboard monitor installed (physical keys → remote)");
}

/// Note a video/monitor window gaining or losing key-window status. Forwarding is enabled
/// while at least one is active; a reference count (not a flag) keeps it correct regardless
/// of the order in which one window's deactivate and another's activate notifications fire.
/// Driven from each window's `connect_is_active_notify` in main.rs.
pub fn note_window_active(active: bool) {
    KB.with(|k| {
        if let Some(s) = &*k.borrow() {
            if active {
                s.active_windows.fetch_add(1, Ordering::Relaxed);
            } else {
                // Saturating decrement: never underflow if notifications are unbalanced.
                let _ = s.active_windows.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
                    Some(c.saturating_sub(1))
                });
            }
        }
    });
}

/// Release every key currently held on the remote. Called on genuine focus loss and by the
/// Ctrl+Alt+G / Ctrl+Alt+P panic shortcuts (which also drop the leaked shortcut modifiers).
pub fn release_all() {
    KB.with(|k| {
        if let Some(s) = &*k.borrow() {
            let held: Vec<u32> = s.pressed.lock().unwrap().drain().collect();
            for keycode in held {
                send_key(&s.writer, keycode, false);
            }
        }
    });
}
