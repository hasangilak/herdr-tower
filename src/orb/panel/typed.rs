//! What is typed into the panel's inputs and not sent yet, kept per input
//! (`crate::unsent`): the message field is the input of the view showing
//! (`input_key`), and shows that input's own text when the view (or the new
//! space in draft) changes, when the window opens again, and after a
//! restart; what it holds is written down as it changes, and goes once it
//! is sent. A card's "Other" field keeps its words the same way (see
//! `questions`).

use gpui::{App, Context, Window};
use gpui_kit::component::input::{InlineToken, InputContent};

use super::Panel;
use crate::orb::nav::Scope;
use crate::unsent::{self, Draft, Key, Token};

/// An input's content as its draft; None when it holds nothing.
pub(super) fn draft_of(c: &InputContent) -> Option<Draft> {
    let d = Draft {
        text: c.text().to_string(),
        tokens: c
            .tokens()
            .iter()
            .map(|s| Token { start: s.range().start, end: s.range().end, id: s.token().id().to_string(), text: s.token().text().to_string() })
            .collect(),
        at_ms: 0,
    };
    (!d.is_empty()).then_some(d)
}

/// A draft as an input's content: its words, and its files where they were
/// (a token that no longer fits its place is left as words).
pub(super) fn content_of(d: &Draft) -> InputContent {
    let mut c = InputContent::new(d.text.clone());
    for t in &d.tokens {
        if let Ok(with) = c.clone().with_token(t.start..t.end, InlineToken::new(t.id.clone(), t.text.clone())) {
            c = with;
        }
    }
    c
}

impl Panel {
    /// Which input the message field is now: the floating panel's and All
    /// spaces' are the main tower's; a space's, a session's, the behaviors'
    /// and each new space in draft have their own.
    pub(super) fn input_key(&self, cx: &App) -> String {
        let Some(full) = self.full.as_ref() else { return Key::MAIN.into() };
        match &self.tower.read(cx).nav.scope {
            Scope::Space(name) => Key::space(name),
            Scope::Session { addr, .. } => Key::session(addr),
            Scope::New => Key::new_space(full.launch.draft_id()),
            Scope::Behaviors => Key::BEHAVIORS.into(),
            _ => Key::MAIN.into(),
        }
    }

    /// The message field shows the text of the input it is now: when that
    /// changed (another view, another new space in draft, the window just
    /// opened), its own kept text comes back into it.
    pub(super) fn follow_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.input_key(cx);
        if key == self.draft_key {
            return;
        }
        let content = unsent::get(&key).map(|d| content_of(&d)).unwrap_or_else(|| InputContent::new(""));
        self.draft_key = key;
        self.next = content.tokens().len() + 1;
        if self.text.read(cx).content() != content {
            // The caret at its end: what is typed next goes on from there.
            let (field, end) = (self.text.clone(), content.text().len());
            self.set_content(&field, content, end, window, cx);
        }
    }

    /// What the message field holds is kept as its input's draft (emptied:
    /// the draft goes), on each Change it says. Not a take the voice writes
    /// into an empty field, nor the words of one on its way. (A value set
    /// from here says no Change: a sent message's draft goes by name.)
    pub(super) fn keep_draft(&self, cx: &App) {
        if self.draft_key.is_empty() {
            return;
        }
        let content = self.text.read(cx).content();
        let own_voice = self.voice.is_some() && self.base.text().trim().is_empty();
        let parting = self.parting.as_deref() == Some(content.text().as_ref());
        if own_voice || parting {
            return;
        }
        unsent::put(&self.draft_key, draft_of(&content));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words and files come back as they were typed; nothing is no draft.
    #[test]
    fn a_fields_content_and_its_draft_are_the_same() {
        let c = InputContent::new("see [Image #1] please").with_token(4..14, InlineToken::new("/tmp/a.png", "[Image #1]")).unwrap();
        let d = draft_of(&c).unwrap();
        assert_eq!((d.text.as_str(), d.tokens.len(), d.tokens[0].id.as_str(), d.tokens[0].start, d.tokens[0].end), ("see [Image #1] please", 1, "/tmp/a.png", 4, 14));
        assert_eq!(content_of(&d), c);
        assert_eq!(draft_of(&InputContent::new("   ")), None);
        // A token whose place no longer holds its label (the file was kept, the words weren't): words only.
        let off = Draft { text: "see it".into(), tokens: vec![Token { start: 4, end: 14, id: "/tmp/a.png".into(), text: "[Image #1]".into() }], at_ms: 0 };
        assert_eq!(content_of(&off), InputContent::new("see it"));
    }
}
