//! The few AppKit calls GPUI has no API for: where the orb's window is,
//! moving it, and keeping it out of the Dock. Positions are AppKit screen
//! points (origin bottom-left of the main screen), the same on every display.

#![allow(unexpected_cfgs)] // objc 0.2 macros test a `cargo-clippy` feature.

use cocoa::base::{NO, YES, id, nil};
use cocoa::foundation::{NSPoint, NSRect};
use objc::{class, msg_send, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// A window's NSWindow, and GPUI's view in it. They live as long as the
/// GPUI window that owns them.
#[derive(Clone, Copy)]
pub struct Win(id, id);

impl Win {
    pub fn of(window: &gpui::Window) -> Option<Self> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(h) = handle.as_raw() else { return None };
        let view = h.ns_view.as_ptr() as id;
        // SAFETY: an NSView GPUI made; -window is a plain getter.
        let w: id = unsafe { msg_send![view, window] };
        (w != nil).then_some(Self(w, view))
    }

    /// Puts a layer (a CAMetalLayer) under GPUI's view, filling the window;
    /// GPUI keeps drawing, and getting the pointer, on top. Returns the
    /// screen's scale (pixels per point).
    pub fn attach_layer_below(self, layer: *mut std::ffi::c_void) -> f64 {
        // SAFETY: AppKit calls on the main thread, on live objects; the new
        // view is retained by its superview.
        unsafe {
            let bounds: NSRect = msg_send![self.1, frame];
            let host: id = msg_send![class!(NSView), alloc];
            let host: id = msg_send![host, initWithFrame: bounds];
            let _: () = msg_send![host, setLayer: layer as id];
            let _: () = msg_send![host, setWantsLayer: YES];
            // NSViewWidthSizable | NSViewHeightSizable
            let _: () = msg_send![host, setAutoresizingMask: 18u64];
            let superview: id = msg_send![self.1, superview];
            // NSWindowBelow
            let _: () = msg_send![superview, addSubview: host positioned: -1i64 relativeTo: self.1];
            let _: () = msg_send![host, release];
            let scale: f64 = msg_send![self.0, backingScaleFactor];
            scale
        }
    }

    pub fn frame(self) -> Rect {
        // SAFETY: -frame on a live NSWindow.
        let f: NSRect = unsafe { msg_send![self.0, frame] };
        Rect { x: f.origin.x, y: f.origin.y, w: f.size.width, h: f.size.height }
    }

    /// Ordered in (on a screen or not).
    pub fn is_visible(self) -> bool {
        // SAFETY: -isVisible on a live NSWindow.
        let v: cocoa::base::BOOL = unsafe { msg_send![self.0, isVisible] };
        v != NO
    }

    pub fn set_origin(self, x: f64, y: f64) {
        // SAFETY: -setFrameOrigin: on a live NSWindow, on the main thread.
        unsafe {
            let _: () = msg_send![self.0, setFrameOrigin: NSPoint::new(x, y)];
        }
    }

    /// No shadow (it would outline the transparent square), and optionally
    /// clicks pass straight through.
    pub fn plain(self, click_through: bool) {
        // SAFETY: NSWindow setters on the main thread.
        unsafe {
            let _: () = msg_send![self.0, setHasShadow: NO];
            if click_through {
                let _: () = msg_send![self.0, setIgnoresMouseEvents: YES];
            }
        }
    }

    /// Frosted glass: rounds the window's content (GPUI's view and the blur
    /// view it puts behind it for `WindowBackgroundAppearance::Blurred`) to
    /// `radius`, and redraws the window's shadow to the rounded shape. The
    /// blur is GPUI's, untinted; the panel adds its own tint.
    pub fn glass(self, radius: f64) {
        // SAFETY: AppKit calls on the main thread, on live objects.
        unsafe {
            let content: id = msg_send![self.0, contentView];
            let _: () = msg_send![content, setWantsLayer: YES];
            let layer: id = msg_send![content, layer];
            let _: () = msg_send![layer, setCornerRadius: radius];
            let _: () = msg_send![layer, setMasksToBounds: YES];
            let _: () = msg_send![self.0, setHasShadow: YES];
            let _: () = msg_send![self.0, invalidateShadow];
        }
    }

    /// Its edges resize it. A window with no title bar isn't resizable
    /// otherwise; its buttons stay hidden (the title bar is invisible).
    pub fn resizable(self) {
        // NSWindowStyleMaskResizable.
        const RESIZABLE: u64 = 1 << 3;
        // SAFETY: as above; `standardWindowButton:` answers nil for a button the window lacks.
        unsafe {
            let mask: u64 = msg_send![self.0, styleMask];
            let _: () = msg_send![self.0, setStyleMask: mask | RESIZABLE];
            // Close, minimise, zoom.
            for kind in 0u64..3 {
                let button: id = msg_send![self.0, standardWindowButton: kind];
                if button != nil {
                    let _: () = msg_send![button, setHidden: YES];
                }
            }
        }
    }

    /// On screen and taking the keyboard (a panel does without activating the app).
    pub fn make_key(self) {
        // SAFETY: as above.
        unsafe {
            let _: () = msg_send![self.0, makeKeyAndOrderFront: nil];
        }
    }

    /// Out of sight, kept as it is (`make_key` or `order_front` brings it back).
    pub fn order_out(self) {
        // SAFETY: as above.
        unsafe {
            let _: () = msg_send![self.0, orderOut: nil];
        }
    }

    pub fn order_front(self) {
        // SAFETY: as above.
        unsafe {
            let _: () = msg_send![self.0, orderFrontRegardless];
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    /// The same, `by` points larger on every side.
    pub fn grown(&self, by: f64) -> Rect {
        Rect { x: self.x - by, y: self.y - by, w: self.w + 2.0 * by, h: self.h + 2.0 * by }
    }
}

/// No Dock icon and no menu bar: the orb is all there is. (GPUI makes every
/// app a regular one when it finishes launching; this runs after.)
pub fn accessory_app() {
    // SAFETY: NSApplication's shared instance; 1 is NSApplicationActivationPolicyAccessory.
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, setActivationPolicy: 1i64];
    }
}

/// Hands the keyboard back to the app that had it before the panel opened.
pub fn give_back_focus() {
    // SAFETY: NSApplication's shared instance, on the main thread.
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![app, deactivate];
    }
}

/// The pointer, in screen points.
pub fn mouse() -> (f64, f64) {
    // SAFETY: a class method returning a struct.
    let p: NSPoint = unsafe { msg_send![class!(NSEvent), mouseLocation] };
    (p.x, p.y)
}

/// Each screen's visible frame (without the menu bar and Dock); the main one first.
pub fn screens() -> Vec<Rect> {
    let mut out = vec![];
    // SAFETY: +[NSScreen screens] is an NSArray of NSScreen.
    unsafe {
        let screens: id = msg_send![class!(NSScreen), screens];
        let n: usize = msg_send![screens, count];
        for i in 0..n {
            let s: id = msg_send![screens, objectAtIndex: i];
            let f: NSRect = msg_send![s, visibleFrame];
            out.push(Rect { x: f.origin.x, y: f.origin.y, w: f.size.width, h: f.size.height });
        }
    }
    out
}

/// The visible frame of the screen holding a point, else the main screen's.
pub fn screen_at(x: f64, y: f64) -> Option<Rect> {
    let all = screens();
    all.iter().find(|s| s.contains(x, y)).or(all.first()).copied()
}
