//! The talk key: Control, on its own, anywhere on the Mac.
//!
//! - Tap Control: start a take; it ends when you pause (tap again to send now).
//! - Hold Control: talk while holding; letting go sends.
//! - Control with any other key or a click (ctrl-c, ctrl-r, ...) is a shortcut,
//!   never a take.
//! - The tower stops talking the moment Control goes down. If it was a
//!   shortcut after all, it carries on; a take drops what it was saying.
//!
//! A listen-only event tap (it never changes or swallows a key). macOS asks
//! once for Input Monitoring, for Herdr Core, since the voice runs in a Herdr
//! terminal.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use core_foundation::runloop::CFRunLoop;
use core_graphics::event::{
    CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType, CallbackResult, EventField,
};

use super::mic::{Ctl, Ears};
use super::state::{Shared, lock};
use super::tts::{Hold, Mouth};

const HOLD: Duration = Duration::from_millis(400);
const LEFT_CONTROL: i64 = 59;
const RIGHT_CONTROL: i64 = 62;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGRequestListenEventAccess() -> bool;
}

#[derive(Default)]
struct Key {
    down_at: Option<Instant>,
    /// Another key, modifier or click came with it: a shortcut.
    other: bool,
    /// Held long enough: a take is running until release.
    holding: bool,
}

/// `which`: `ctrl` (either), `left_ctrl`, `right_ctrl` or `off`.
pub fn start(which: &str, ears: Ears, shared: Shared, mouth: Mouth) {
    let codes: Vec<i64> = match which {
        "off" | "" => {
            shared.update(|s| s.talk_key = "off".into());
            return;
        }
        "left_ctrl" => vec![LEFT_CONTROL],
        "right_ctrl" => vec![RIGHT_CONTROL],
        _ => vec![LEFT_CONTROL, RIGHT_CONTROL],
    };
    let key = Arc::new(Mutex::new(Key::default()));

    // Holding: start the take once Control has been down alone long enough.
    {
        let key = key.clone();
        let ears = ears.clone();
        thread::Builder::new()
            .name("hotkey-hold".into())
            .spawn(move || {
                loop {
                    thread::sleep(Duration::from_millis(30));
                    let mut k = lock(&key);
                    if let Some(t) = k.down_at
                        && !k.other
                        && !k.holding
                        && t.elapsed() >= HOLD
                    {
                        k.holding = true;
                        eprintln!("[hotkey] held: talking until release");
                        ears.control(Ctl::Talk("hold".into()));
                    }
                }
            })
            .ok();
    }

    let label = which.to_string();
    thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            let mut asked = false;
            loop {
                // SAFETY: plain CoreGraphics queries with no arguments.
                let allowed = unsafe { CGPreflightListenEventAccess() };
                if !allowed && !asked {
                    asked = true;
                    // Opens the macOS prompt (once); the setting lives in
                    // System Settings > Privacy & Security > Input Monitoring.
                    unsafe { CGRequestListenEventAccess() };
                }
                let k = key.clone();
                let e = ears.clone();
                let m = mouth.clone();
                let sh = shared.clone();
                let codes = codes.clone();
                let tap = CGEventTap::new(
                    CGEventTapLocation::Session,
                    CGEventTapPlacement::TailAppendEventTap,
                    CGEventTapOptions::ListenOnly,
                    vec![
                        CGEventType::FlagsChanged,
                        CGEventType::KeyDown,
                        CGEventType::LeftMouseDown,
                        CGEventType::RightMouseDown,
                        CGEventType::OtherMouseDown,
                    ],
                    move |_proxy, etype, event| {
                        on_event(&k, &e, &m, &sh, &codes, etype, event);
                        CallbackResult::Keep
                    },
                );
                match tap {
                    Ok(tap) => {
                        let source = tap.mach_port().create_runloop_source(0).expect("run loop source");
                        // SAFETY: kCFRunLoopCommonModes is a constant CFString.
                        CFRunLoop::get_current().add_source(&source, unsafe { core_foundation::runloop::kCFRunLoopCommonModes });
                        tap.enable();
                        eprintln!("[hotkey] {label}: tap or hold Control to talk (Input Monitoring {})", if unsafe { CGPreflightListenEventAccess() } { "allowed" } else { "not yet allowed" });
                        shared.update(|s| s.talk_key = label.clone());
                        CFRunLoop::run_current();
                        // The run loop only returns if the tap went away.
                        drop(tap);
                    }
                    Err(()) => {
                        shared.update(|s| {
                            s.talk_key = "needs-permission".into();
                        });
                        if !asked {
                            eprintln!("[hotkey] no Input Monitoring permission");
                        }
                    }
                }
                thread::sleep(Duration::from_secs(5));
            }
        })
        .ok();
}

fn on_event(
    key: &Mutex<Key>,
    ears: &Ears,
    mouth: &Mouth,
    shared: &Shared,
    codes: &[i64],
    etype: CGEventType,
    event: &core_graphics::event::CGEvent,
) {
    let mut k = lock(key);
    match etype {
        CGEventType::FlagsChanged => {
            let code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
            let flags = event.get_flags();
            let control = flags.contains(CGEventFlags::CGEventFlagControl);
            let others = flags.intersects(
                CGEventFlags::CGEventFlagCommand
                    | CGEventFlags::CGEventFlagShift
                    | CGEventFlags::CGEventFlagAlternate
                    | CGEventFlags::CGEventFlagSecondaryFn,
            );
            if codes.contains(&code) {
                if control && k.down_at.is_none() {
                    *k = Key { down_at: Some(Instant::now()), other: others, holding: false };
                    // Quiet at once: the user is about to talk.
                    if !others {
                        mouth.hold(Hold::Key, true);
                    }
                } else if !control && let Some(down) = k.down_at.take() {
                    if k.holding {
                        eprintln!("[hotkey] released: send");
                        ears.control(Ctl::Talk("stop".into()));
                    } else if !k.other && down.elapsed() < HOLD {
                        eprintln!("[hotkey] tap: talk");
                        // A take is starting: keep quiet until it has, so the
                        // paused sentence doesn't resume for a moment first.
                        if shared.with(|s| s.mic != "recording") {
                            mouth.hold(Hold::Take, true);
                        }
                        ears.control(Ctl::Talk("toggle".into()));
                    }
                    mouth.hold(Hold::Key, false);
                    k.holding = false;
                    k.other = false;
                }
            } else if k.down_at.is_some() {
                shortcut(&mut k, ears, mouth);
            }
        }
        _ => {
            if k.down_at.is_some() {
                shortcut(&mut k, ears, mouth);
            }
        }
    }
}

/// Control was part of a shortcut: the tower carries on talking, and a take
/// the hold may have started is dropped.
fn shortcut(k: &mut Key, ears: &Ears, mouth: &Mouth) {
    k.other = true;
    mouth.hold(Hold::Key, false);
    if k.holding {
        k.holding = false;
        ears.control(Ctl::Cancel);
    }
}
