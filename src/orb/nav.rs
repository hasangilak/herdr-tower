//! Where the full window is (`docs/quiet-tower.md`, "Steps 4 and 6"; W1 to
//! W20 in the design): the view its main pane shows, which row of the sidebar
//! that lights, and the way back from a view that has one.
//!
//! - `Scope` is the view: every space, one space, the launcher, the record,
//!   the screenshots, the settings and what opens from them (behaviors,
//!   pairing a phone), or one session of a space.
//! - `Nav` is the scope with what goes with it: the record's space filter
//!   (opened from inside a space it starts on that space), the quiet fold,
//!   and a line said once at the top of a view (a space launched, a space
//!   closed).
//! - It is kept with the tower's state, so the floating panel can send the
//!   window somewhere (a space's chip opens that space) and the window is
//!   where it was when it opens again.
//!
//! Nothing here draws or does I/O.

use super::mac::Rect;

/// A space the launcher just made stays open this long before Herdr lists it (ms).
const LAUNCH_GRACE_MS: u64 = 30_000;

/// What the main pane shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Scope {
    /// The queue and the missions: the main tower.
    #[default]
    All,
    /// One space: its own tower.
    Space(String),
    /// The launcher.
    New,
    Record,
    Shots,
    Settings,
    /// From Settings.
    Behaviors,
    /// From Settings: pairing the phone app.
    Phone,
    /// One session, from its space (`space` is empty when it has none).
    Session { space: String, addr: String },
}

/// The row of the sidebar a scope lights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    All,
    Space(String),
    New,
    Record,
    Shots,
    Settings,
}

impl Scope {
    /// The sidebar's row for it: a session lights its space, and what opens
    /// from Settings lights Settings.
    pub fn side(&self) -> Side {
        match self {
            Scope::All => Side::All,
            Scope::Space(name) => Side::Space(name.clone()),
            Scope::Session { space, .. } if !space.is_empty() => Side::Space(space.clone()),
            Scope::Session { .. } => Side::All,
            Scope::New => Side::New,
            Scope::Record => Side::Record,
            Scope::Shots => Side::Shots,
            Scope::Settings | Scope::Behaviors | Scope::Phone => Side::Settings,
        }
    }

    /// The way back, for a view that was opened from another: its button's
    /// words and where it leads.
    pub fn back(&self) -> Option<(String, Scope)> {
        match self {
            Scope::Behaviors | Scope::Phone => Some(("Back to Settings".into(), Scope::Settings)),
            Scope::Session { space, .. } if !space.is_empty() => Some((format!("Back to {space}"), Scope::Space(space.clone()))),
            Scope::Session { .. } => Some(("Back to All spaces".into(), Scope::All)),
            _ => None,
        }
    }

    /// The number keys answer the open card here: where a queue shows.
    pub fn answers(&self) -> bool {
        matches!(self, Scope::All | Scope::Space(_))
    }

    /// The space whose tower the input talks to (none: the main tower).
    pub fn space(&self) -> Option<&str> {
        match self {
            Scope::Space(name) => Some(name),
            _ => None,
        }
    }

    /// A scope by its word, as `HERDER_TOWER_ORB_WINDOW` names it: `all`,
    /// `new`, `record`, `record=<space>`, `shots`, `settings`, `behaviors`,
    /// `phone`, `space=<name>`, `session=<address>` or
    /// `session=<space>/<address>`. With it, the record's filter.
    pub fn parse(word: &str) -> (Scope, Option<String>) {
        let (key, value) = match word.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (word.trim(), ""),
        };
        match (key, value) {
            ("new", _) => (Scope::New, None),
            ("record", "") => (Scope::Record, None),
            ("record", space) => (Scope::Record, Some(space.to_string())),
            ("shots", _) => (Scope::Shots, None),
            ("settings", _) => (Scope::Settings, None),
            ("behaviors", _) => (Scope::Behaviors, None),
            ("phone", _) => (Scope::Phone, None),
            ("space", name) if !name.is_empty() => (Scope::Space(name.to_string()), None),
            ("session", at) if !at.is_empty() => {
                let (space, addr) = at.rsplit_once('/').unwrap_or(("", at));
                (Scope::Session { space: space.to_string(), addr: addr.to_string() }, None)
            }
            _ => (Scope::All, None),
        }
    }
}

/// The full window's place in the tower.
#[derive(Debug, Default)]
pub struct Nav {
    pub scope: Scope,
    /// The record shows this space only (none: every space).
    pub rec_filter: Option<String>,
    /// A goal the launcher opens on (`HERDER_TOWER_ORB_WINDOW=show:new=<goal>`),
    /// until the window has taken it.
    pub goal: Option<String>,
    /// The quiet spaces' names are showing.
    pub quiet_open: bool,
    /// Said once at the top of a view: where, and what.
    notice: Option<(Scope, String)>,
    /// The act the notice is the receipt of (what a launch opened): it is
    /// undone from there, as from a reply.
    notice_act: Option<String>,
    /// A space the launcher just made, until Herdr lists it.
    launched: Option<Launched>,
}

#[derive(Debug)]
struct Launched {
    /// The name the launcher expects it to get.
    name: String,
    at: u64,
    /// The spaces there were before, to tell it by if it got another name.
    before: Vec<String>,
}

impl Nav {
    /// To another view. The record opened from inside a space shows that
    /// space; a notice stays only where it was said.
    pub fn go(&mut self, to: Scope) {
        if to == Scope::Record && self.scope != Scope::Record {
            self.rec_filter = match &self.scope {
                Scope::Space(name) => Some(name.clone()),
                Scope::Session { space, .. } if !space.is_empty() => Some(space.clone()),
                _ => None,
            };
        }
        if self.notice.as_ref().is_some_and(|(at, _)| *at != to) {
            (self.notice, self.notice_act) = (None, None);
        }
        self.scope = to;
    }

    /// Goes to `at` and says `text` at its top.
    pub fn say(&mut self, at: Scope, text: String) {
        (self.notice, self.notice_act) = (Some((at.clone(), text)), None);
        self.go(at);
    }

    /// What is said at the top of the view showing.
    pub fn notice(&self) -> Option<&str> {
        self.notice.as_ref().filter(|(at, _)| *at == self.scope).map(|(_, text)| text.as_str())
    }

    /// What is said there is the receipt of that act (`tower launch`'s
    /// `act`): Undo is offered with it while it shows.
    pub fn receipt_of(&mut self, act: Option<String>) {
        self.notice_act = act.filter(|a| !a.is_empty() && self.notice.is_some());
    }

    /// The act whose receipt is said at the top of the view showing.
    pub fn notice_act(&self) -> Option<&str> {
        self.notice().and(self.notice_act.as_deref())
    }

    /// A space was launched: its view opens under the name the launcher
    /// expects (`before`: the spaces there were), and stays while Herdr
    /// doesn't list it yet.
    pub fn launched(&mut self, name: &str, before: Vec<String>, text: String, now_ms: u64) {
        self.launched = Some(Launched { name: name.to_string(), at: now_ms, before });
        self.say(Scope::Space(name.to_string()), text);
    }

    /// The space the launcher made that isn't listed yet.
    pub fn starting(&self, now_ms: u64) -> Option<&str> {
        self.launched.as_ref().filter(|l| now_ms.saturating_sub(l.at) < LAUNCH_GRACE_MS).map(|l| l.name.as_str())
    }

    /// Looks at the open spaces again: a view of a space that is gone
    /// (closed, emptied) falls back to every space, and a launched space
    /// that got another name than expected (the one new space there is) is
    /// followed to it. True when the view moved.
    pub fn settle(&mut self, open: &[&str], now_ms: u64) -> bool {
        if let Some(l) = &self.launched {
            let fresh: Vec<&str> = open.iter().copied().filter(|n| !l.before.iter().any(|b| b == n)).collect();
            let renamed = (!open.contains(&l.name.as_str()) && fresh.len() == 1).then(|| (l.name.clone(), fresh[0].to_string()));
            if let Some((expected, actual)) = renamed {
                let was = Scope::Space(expected);
                self.launched = None;
                if let Some((at, _)) = self.notice.as_mut().filter(|(at, _)| *at == was) {
                    *at = Scope::Space(actual.clone());
                }
                if self.scope == was {
                    self.scope = Scope::Space(actual);
                    return true;
                }
            }
        }
        if self.launched.as_ref().is_some_and(|l| open.contains(&l.name.as_str()) || now_ms.saturating_sub(l.at) >= LAUNCH_GRACE_MS) {
            self.launched = None;
        }
        let gone = match &self.scope {
            Scope::Space(name) => !open.contains(&name.as_str()) && self.starting(now_ms) != Some(name.as_str()),
            _ => false,
        };
        if gone {
            self.go(Scope::All);
        }
        gone
    }
}

/// Where the full window goes on `screen`: beside `anchor` (the floating
/// panel) on whichever side has room, level with it; else, or with no
/// anchor, in the middle. Always wholly on the screen. AppKit points.
pub fn place(anchor: Option<Rect>, screen: Rect, w: f64, h: f64) -> (f64, f64) {
    const GAP: f64 = 8.0;
    let clamp = |v: f64, low: f64, high: f64| v.min(high).max(low);
    let middle = (screen.x + (screen.w - w) / 2.0, screen.y + (screen.h - h) / 2.0);
    let (x, y) = match anchor {
        Some(a) if a.x - GAP - w >= screen.x => (a.x - GAP - w, a.y + a.h - h),
        Some(a) if a.x + a.w + GAP + w <= screen.x + screen.w => (a.x + a.w + GAP, a.y + a.h - h),
        _ => middle,
    };
    (clamp(x, screen.x, screen.x + screen.w - w), clamp(y, screen.y, screen.y + screen.h - h))
}

/// Where the full window goes when it takes the floating panel's place
/// (`was`, which closes): over it, its side toward the orb and its top level
/// with the panel's, so the orb's corner of the screen stays where it was.
/// Wholly on the screen. AppKit points.
pub fn place_over(was: Rect, screen: Rect, w: f64, h: f64) -> (f64, f64) {
    let clamp = |v: f64, low: f64, high: f64| v.min(high).max(low);
    // The panel opens on the orb's left when there is room: its right edge is the one toward the orb.
    let toward_right = was.x + was.w / 2.0 >= screen.x + screen.w / 2.0;
    let x = if toward_right { was.x + was.w - w } else { was.x };
    let y = was.y + was.h - h;
    (clamp(x, screen.x, screen.x + screen.w - w), clamp(y, screen.y, screen.y + screen.h - h))
}

/// The full window's size: the one the user last gave it (`saved`), else
/// `default`; never under `least`, never over the screen.
pub fn full_size(saved: Option<(f64, f64)>, screen: Rect, default: (f64, f64), least: (f64, f64)) -> (f64, f64) {
    let (w, h) = saved.filter(|(w, h)| w.is_finite() && h.is_finite() && *w > 0.0 && *h > 0.0).unwrap_or(default);
    (w.min(screen.w).max(least.0), h.min(screen.h).max(least.1))
}

/// A window of `w` by `h` at `(x, y)`, moved as little as it takes to be
/// wholly on a screen: the one its middle is on, else the nearest. With no
/// screens, as it is. AppKit points.
pub fn keep_on(screens: &[Rect], x: f64, y: f64, w: f64, h: f64) -> (f64, f64) {
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let far = |s: &Rect| {
        let dx = (s.x - cx).max(cx - (s.x + s.w)).max(0.0);
        let dy = (s.y - cy).max(cy - (s.y + s.h)).max(0.0);
        dx.hypot(dy)
    };
    let Some(s) = screens.iter().min_by(|a, b| far(a).total_cmp(&far(b))) else { return (x, y) };
    // A window larger than the screen keeps its top left corner on it.
    let clamp = |v: f64, low: f64, high: f64| v.min(high.max(low)).max(low);
    (clamp(x, s.x, s.x + s.w - w), if h > s.h { s.y + s.h - h } else { clamp(y, s.y, s.y + s.h - h) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_dropped_off_the_edge_comes_back_on() {
        let main = Rect { x: 0.0, y: 0.0, w: 1440.0, h: 875.0 };
        let left = Rect { x: -1920.0, y: 100.0, w: 1920.0, h: 1055.0 };
        let both = [main, left];
        // Wholly on a screen: where the user put it.
        assert_eq!(keep_on(&both, 200.0, 100.0, 960.0, 640.0), (200.0, 100.0));
        assert_eq!(keep_on(&both, -1500.0, 300.0, 960.0, 640.0), (-1500.0, 300.0), "on the second screen");
        // Over an edge: back by as much as it hangs over, on the screen its middle is on.
        assert_eq!(keep_on(&[main], 900.0, -200.0, 960.0, 640.0), (480.0, 0.0));
        assert_eq!(keep_on(&[main], -300.0, 600.0, 960.0, 640.0), (0.0, 235.0));
        assert_eq!(keep_on(&both, -400.0, 300.0, 960.0, 640.0), (0.0, 235.0), "its middle is on the main screen");
        assert_eq!(keep_on(&both, -600.0, 300.0, 960.0, 640.0), (-960.0, 300.0), "its middle is on the left one");
        // Off every screen (one was unplugged): onto the nearest.
        assert_eq!(keep_on(&[main], -3000.0, 2000.0, 960.0, 640.0), (0.0, 235.0));
        // A screen smaller than the window keeps its top left corner; no screens: as it is.
        let small = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
        assert_eq!(keep_on(&[small], 50.0, 50.0, 960.0, 640.0), (0.0, -40.0));
        assert_eq!(keep_on(&[], 5.0, 6.0, 960.0, 640.0), (5.0, 6.0));
    }

    fn session(space: &str, addr: &str) -> Scope {
        Scope::Session { space: space.into(), addr: addr.into() }
    }

    #[test]
    fn a_view_lights_its_row_of_the_sidebar() {
        assert_eq!(Scope::All.side(), Side::All);
        assert_eq!(Scope::Space("isa".into()).side(), Side::Space("isa".into()));
        assert_eq!(session("isa", "t3").side(), Side::Space("isa".into()), "a session lights its space");
        assert_eq!(session("", "t3").side(), Side::All);
        assert_eq!(Scope::New.side(), Side::New);
        for s in [Scope::Settings, Scope::Behaviors, Scope::Phone] {
            assert_eq!(s.side(), Side::Settings, "{s:?}");
        }
        assert_eq!((Scope::Record.side(), Scope::Shots.side()), (Side::Record, Side::Shots));
    }

    #[test]
    fn what_opens_from_another_view_has_a_way_back() {
        assert_eq!(Scope::Behaviors.back(), Some(("Back to Settings".into(), Scope::Settings)));
        assert_eq!(Scope::Phone.back(), Some(("Back to Settings".into(), Scope::Settings)));
        assert_eq!(session("isa", "t3").back(), Some(("Back to isa".into(), Scope::Space("isa".into()))));
        assert_eq!(session("", "t3").back(), Some(("Back to All spaces".into(), Scope::All)));
        for s in [Scope::All, Scope::Space("isa".into()), Scope::New, Scope::Record, Scope::Shots, Scope::Settings] {
            assert_eq!(s.back(), None, "{s:?}");
        }
    }

    #[test]
    fn number_keys_count_where_a_queue_shows() {
        assert!(Scope::All.answers() && Scope::Space("isa".into()).answers());
        for s in [Scope::New, Scope::Record, Scope::Shots, Scope::Settings, Scope::Behaviors, Scope::Phone, session("isa", "t3")] {
            assert!(!s.answers(), "{s:?}");
        }
        // The input talks to a space's tower only in that space's view.
        assert_eq!(Scope::Space("isa".into()).space(), Some("isa"));
        assert_eq!((Scope::All.space(), session("isa", "t3").space()), (None, None));
    }

    #[test]
    fn the_record_opened_from_a_space_shows_that_space() {
        let mut nav = Nav::default();
        nav.go(Scope::Record);
        assert_eq!(nav.rec_filter, None, "from every space: every space");
        nav.go(Scope::Space("sandbox".into()));
        nav.go(Scope::Record);
        assert_eq!(nav.rec_filter.as_deref(), Some("sandbox"));
        // A chip picked there stays when Record is clicked again.
        nav.rec_filter = Some("isa".into());
        nav.go(Scope::Record);
        assert_eq!(nav.rec_filter.as_deref(), Some("isa"));
        // From a session, its space; from anywhere else, all of it.
        nav.go(session("anna", "t9"));
        nav.go(Scope::Record);
        assert_eq!(nav.rec_filter.as_deref(), Some("anna"));
        nav.go(Scope::Settings);
        nav.go(Scope::Record);
        assert_eq!(nav.rec_filter, None);
    }

    #[test]
    fn a_notice_is_said_once_where_it_belongs() {
        let mut nav = Nav { scope: Scope::Space("anna".into()), ..Default::default() };
        nav.say(Scope::All, "anna is closed.".into());
        assert_eq!((nav.scope.clone(), nav.notice()), (Scope::All, Some("anna is closed.")));
        nav.go(Scope::All);
        assert_eq!(nav.notice(), Some("anna is closed."), "the same view again keeps it");
        nav.go(Scope::Shots);
        assert_eq!(nav.notice(), None);
        nav.go(Scope::All);
        assert_eq!(nav.notice(), None, "gone once you looked away");
    }

    #[test]
    fn a_space_that_is_gone_falls_back_to_every_space() {
        let now = 1_000_000;
        let mut nav = Nav { scope: Scope::Space("anna".into()), ..Default::default() };
        assert!(!nav.settle(&["anna", "isa"], now));
        assert!(nav.settle(&["isa"], now));
        assert_eq!(nav.scope, Scope::All);
        // Just launched: its view waits for Herdr to list it.
        nav.launched("case-exports", vec!["isa".into()], "Launched case-exports.".into(), now);
        assert_eq!((nav.scope.clone(), nav.starting(now + 5000)), (Scope::Space("case-exports".into()), Some("case-exports")));
        assert!(!nav.settle(&["isa"], now + 5000));
        assert_eq!(nav.notice(), Some("Launched case-exports."));
        // Its receipt names the act it kept: Undo shows with it, and goes with it.
        assert_eq!(nav.notice_act(), None);
        nav.receipt_of(Some("r0123456789ab".into()));
        assert_eq!(nav.notice_act(), Some("r0123456789ab"));
        // Listed: an ordinary space from then on.
        assert!(!nav.settle(&["isa", "case-exports"], now + 6000));
        assert_eq!(nav.starting(now + 6000), None);
        assert!(nav.settle(&["isa"], now + 7000), "and gone like any other once it closes");
        // Never listed: it gives up.
        nav.launched("ghost", vec![], String::new(), now);
        assert!(!nav.settle(&[], now + LAUNCH_GRACE_MS - 1));
        assert!(nav.settle(&[], now + LAUNCH_GRACE_MS));
        // The tower gave it another name: the one new space is it, notice and all.
        nav.launched("case-exports", vec!["isa".into(), "anna".into()], "Launched.".into(), now);
        assert!(!nav.settle(&["isa", "anna"], now + 1000), "not there yet");
        assert!(nav.settle(&["isa", "anna", "case-exports-pdf"], now + 2000));
        assert_eq!((nav.scope.clone(), nav.notice()), (Scope::Space("case-exports-pdf".into()), Some("Launched.")));
        assert_eq!(nav.starting(now + 2000), None);
        // Two new spaces at once: no guessing which.
        nav.launched("x", vec!["isa".into()], String::new(), now);
        assert!(!nav.settle(&["isa", "a", "b"], now + 1000));
        assert_eq!(nav.scope, Scope::Space("x".into()));
        nav.go(Scope::All);
        // Views that are no space's are never moved.
        nav.go(session("anna", "t9"));
        assert!(!nav.settle(&[], now));
    }

    #[test]
    fn a_scope_reads_from_its_word() {
        assert_eq!(Scope::parse("all"), (Scope::All, None));
        assert_eq!(Scope::parse(""), (Scope::All, None));
        assert_eq!(Scope::parse("new"), (Scope::New, None));
        assert_eq!(Scope::parse("record"), (Scope::Record, None));
        assert_eq!(Scope::parse("record=sandbox"), (Scope::Record, Some("sandbox".into())));
        assert_eq!(Scope::parse("space=M5 Pro"), (Scope::Space("M5 Pro".into()), None));
        assert_eq!(Scope::parse("session=t12"), (session("", "t12"), None));
        assert_eq!(Scope::parse("session=sandbox/t12"), (session("sandbox", "t12"), None));
        assert_eq!(Scope::parse("session=sandbox/vps/t2"), (session("sandbox/vps", "t2"), None));
        assert_eq!((Scope::parse("shots").0, Scope::parse("settings").0), (Scope::Shots, Scope::Settings));
        assert_eq!((Scope::parse("behaviors").0, Scope::parse("phone").0), (Scope::Behaviors, Scope::Phone));
        assert_eq!(Scope::parse("space="), (Scope::All, None));
    }

    #[test]
    fn the_window_goes_beside_the_panel_where_there_is_room() {
        let screen = Rect { x: 0.0, y: 0.0, w: 1512.0, h: 982.0 };
        let (w, h) = (960.0, 640.0);
        // The panel near the right edge: the window to its left, level with it.
        let panel = Rect { x: 1068.0, y: 300.0, w: 380.0, h: 640.0 };
        assert_eq!(place(Some(panel), screen, w, h), (100.0, 300.0));
        // The panel on the left: the window to its right.
        let panel = Rect { x: 60.0, y: 200.0, w: 380.0, h: 640.0 };
        assert_eq!(place(Some(panel), screen, w, h), (448.0, 200.0));
        // No room on either side (a narrow screen): the middle, wholly on the screen.
        let small = Rect { x: 0.0, y: 0.0, w: 1280.0, h: 720.0 };
        let panel = Rect { x: 500.0, y: 40.0, w: 380.0, h: 640.0 };
        assert_eq!(place(Some(panel), small, w, h), (160.0, 40.0));
        assert_eq!(place(None, screen, w, h), (276.0, 171.0));
        // A second screen keeps its own coordinates, and a short one clamps.
        let left = Rect { x: -1920.0, y: 100.0, w: 1920.0, h: 600.0 };
        assert_eq!(place(None, left, w, h), (-1440.0, 100.0));
    }

    #[test]
    fn the_full_window_takes_the_floating_panels_place_at_the_size_it_was_given() {
        let screen = Rect { x: 0.0, y: 0.0, w: 1728.0, h: 1080.0 };
        // The panel left of an orb at the top right: the window's right edge and top are the panel's.
        let panel = Rect { x: 1300.0, y: 380.0, w: 380.0, h: 640.0 };
        assert_eq!(place_over(panel, screen, 960.0, 640.0), (1300.0 + 380.0 - 960.0, 380.0));
        // Taller than the room above the screen's bottom: kept on the screen.
        assert_eq!(place_over(panel, screen, 960.0, 1200.0).1, 0.0);
        // A panel on the left half: its left edge is the one toward the orb.
        let left = Rect { x: 60.0, y: 380.0, w: 380.0, h: 640.0 };
        assert_eq!(place_over(left, screen, 960.0, 640.0).0, 60.0);
        // Its size: the one kept, never under the least, never over the screen; none kept is the default.
        let (default, least) = ((960.0, 640.0), (820.0, 520.0));
        assert_eq!(full_size(None, screen, default, least), (960.0, 640.0));
        assert_eq!(full_size(Some((1400.0, 900.0)), screen, default, least), (1400.0, 900.0));
        assert_eq!(full_size(Some((300.0, 200.0)), screen, default, least), (820.0, 520.0));
        assert_eq!(full_size(Some((4000.0, 3000.0)), screen, default, least), (1728.0, 1080.0));
        assert_eq!(full_size(Some((f64::NAN, 900.0)), screen, default, least), (960.0, 640.0), "a broken file is the default");
    }
}
