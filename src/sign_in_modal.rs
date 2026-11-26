//! The sign-in popup's content and layout, as pure functions of the screen size and the sign-in state. Drawing
//! (`ui::draw_sign_in`) and mouse clicks on its buttons both use [`plan`], so what is clickable is what was drawn.

use crate::app::SignIn;
use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The popup's widest extent, in columns.
const MAX_WIDTH: u16 = 96;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    SignIn,
    Copy,
    Close,
}

/// How a line of the popup is drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Heading,
    Plain,
    Label,
    Detail,
    Failure,
}

#[derive(Debug)]
pub struct Plan {
    /// The popup including its border, as `ui::popup` places it for this size.
    pub outer: Rect,
    /// Where the text goes: inside the border, with a one-column margin each side.
    pub text: Rect,
    pub lines: Vec<(Kind, String)>,
    /// The buttons on the last row of the popup, with the key that does the same.
    pub buttons: [(Button, Rect, &'static str); 3],
}

impl Plan {
    pub fn button_at(&self, column: u16, row: u16) -> Option<Button> {
        self.buttons
            .iter()
            .find(|(_, r, _)| crate::view::Geometry::contains(*r, column, row))
            .map(|(b, _, _)| *b)
    }
}

/// Break `text` into lines at most `width` columns wide: at spaces, and inside a word that is longer than a line (a
/// token, a trace id). Line breaks in `text` are kept.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && line.width() + 1 + word.width() <= width {
                line.push(' ');
                line.push_str(word);
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            for c in word.chars() {
                if line.width() + c.width().unwrap_or(0) > width {
                    out.push(std::mem::take(&mut line));
                }
                line.push(c);
            }
        }
        out.push(line);
    }
    out
}

/// The text the popup is made of for this state. `width` is the room for text.
fn content(state: &SignIn, width: usize) -> Vec<(Kind, String)> {
    let mut lines = Vec::new();
    let mut add = |kind: Kind, text: &str| {
        if text.is_empty() {
            lines.push((kind, String::new()));
        }
        lines.extend(wrap(text, width).into_iter().map(|l| (kind, l)));
    };
    match state {
        SignIn::Waiting { .. } => {
            add(Kind::Heading, "Signing in…");
            add(Kind::Plain, "");
            add(
                Kind::Plain,
                "Finish signing in in the browser window that opened; it may not ask for anything. \
                 vtteams carries on by itself, and what you are typing is kept.",
            );
        }
        SignIn::Expired { failure, .. } => {
            add(
                Kind::Heading,
                if failure.is_some() {
                    "Signing in again did not work"
                } else {
                    "Your Teams sign-in has expired"
                },
            );
            add(Kind::Plain, "");
            add(
                Kind::Plain,
                "Teams signs the web client out every 24 hours. Sign in again to carry on; \
                 what you are typing is kept.",
            );
        }
        SignIn::Fine => {}
    }
    if let Some(reason) = state.reason().filter(|r| !r.is_empty()) {
        add(Kind::Plain, "");
        add(Kind::Label, "Why:");
        add(Kind::Detail, reason);
    }
    if let Some(failure) = state.failure() {
        add(Kind::Plain, "");
        add(Kind::Label, "The last attempt failed:");
        add(Kind::Failure, failure);
    }
    lines
}

/// Everything the popup shows or can be clicked on, in a terminal of size `area`.
pub fn plan(area: Rect, state: &SignIn) -> Plan {
    // The same arithmetic as `ui::popup`, which is handed this width and height.
    let width = area.width.saturating_sub(4).clamp(1, MAX_WIDTH);
    let text_width = usize::from(width.saturating_sub(4)).max(1);
    let mut lines = content(state, text_width);
    // Border (2), a blank row and the buttons (2); the rest is for text.
    let room = usize::from(area.height.saturating_sub(2 + 2 + 2)).max(1);
    if lines.len() > room {
        lines.truncate(room);
        if let Some(last) = lines.last_mut() {
            *last = (Kind::Plain, "… (Copy details has the rest)".into());
        }
    }
    let height = (lines.len() as u16).saturating_add(4);
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    let outer = Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    let inner = Rect {
        x: outer.x + 1,
        y: outer.y + 1,
        width: outer.width.saturating_sub(2),
        height: outer.height.saturating_sub(2),
    };
    let text = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(2),
        height: inner.height.saturating_sub(2),
        ..inner
    };
    let row = inner.bottom().saturating_sub(1);
    let mut x = text.x;
    let labels = [
        (Button::SignIn, "Enter  Sign in again"),
        (Button::Copy, "c  Copy details"),
        (Button::Close, "Esc  Close"),
    ];
    let buttons = labels.map(|(button, label)| {
        let width = (label.width() as u16 + 2).min(text.right().saturating_sub(x));
        let rect = Rect {
            x,
            y: row,
            width,
            height: 1,
        };
        x = x.saturating_add(width + 2);
        (button, rect, label)
    });
    Plan {
        outer,
        text,
        lines,
        buttons,
    }
}

/// What "Copy details" puts on the clipboard: the whole story, for pasting into a note or a bug report.
pub fn details(state: &SignIn) -> String {
    let mut out = String::from("vtteams: the Teams sign-in needs renewing\n");
    if let Some(reason) = state.reason().filter(|r| !r.is_empty()) {
        out.push_str(&format!("\nWhy: {reason}\n"));
    }
    if let Some(failure) = state.failure() {
        out.push_str(&format!("\nThe last attempt failed: {failure}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expired(reason: &str, failure: Option<&str>) -> SignIn {
        SignIn::Expired {
            reason: reason.into(),
            failure: failure.map(String::from),
        }
    }

    #[test]
    fn text_wraps_at_spaces_keeps_breaks_and_splits_a_word_that_cannot_fit() {
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(wrap("one\ntwo", 20), ["one", "two"]);
        assert_eq!(wrap("0123456789abcdef", 6), ["012345", "6789ab", "cdef"]);
        assert_eq!(wrap("ab 0123456789", 6), ["ab", "012345", "6789"]);
        assert_eq!(wrap("日本語日本語", 6), ["日本語", "日本語"]);
        assert!(wrap("", 10).is_empty());
        for line in wrap(
            "some words and a very long-token-without-spaces-in-it here",
            9,
        ) {
            assert!(line.width() <= 9, "{line:?}");
        }
    }

    #[test]
    fn the_popup_shows_what_was_said_and_has_three_buttons_inside_it() {
        let reason = "sign-in rejected (invalid_grant): AADSTS700084: the refresh token expired. Trace ID: dece8b33";
        let state = expired(reason, Some("the browser exited at once"));
        let plan = plan(Rect::new(0, 0, 120, 40), &state);
        let shown: String = plan
            .lines
            .iter()
            .map(|(_, l)| format!("{l} "))
            .collect::<String>();
        for word in ["AADSTS700084", "dece8b33", "browser exited at once"] {
            assert!(shown.contains(word), "{word}: {shown}");
        }
        assert!(plan.outer.width > 40, "{:?}", plan.outer);
        for (_, rect, _) in &plan.buttons {
            assert!(
                rect.width > 0
                    && rect.x > plan.outer.x
                    && rect.right() < plan.outer.right()
                    && rect.y == plan.outer.bottom() - 2,
                "{rect:?} in {:?}",
                plan.outer
            );
        }
        // The text and the button row do not overlap.
        assert!(plan.text.y + plan.lines.len() as u16 <= plan.outer.bottom() - 2);
        let first = plan.buttons[0].1;
        assert_eq!(plan.button_at(first.x, first.y), Some(Button::SignIn));
        assert_eq!(plan.button_at(first.x, first.y - 1), None);
        assert_eq!(
            plan.button_at(plan.buttons[2].1.x, plan.buttons[2].1.y),
            Some(Button::Close)
        );
    }

    #[test]
    fn the_popup_fits_any_terminal_and_cuts_a_very_long_message_short() {
        let long = "word ".repeat(2000);
        let state = expired(&long, Some(&long));
        for (w, h) in [
            (120, 40),
            (80, 24),
            (40, 12),
            (20, 8),
            (10, 6),
            (3, 3),
            (0, 0),
        ] {
            let area = Rect::new(0, 0, w, h);
            let plan = plan(area, &state);
            assert!(
                plan.outer.right() <= area.right() && plan.outer.bottom() <= area.bottom(),
                "{w}x{h}: {:?}",
                plan.outer
            );
            assert!(plan.lines.len() <= usize::from(h.saturating_sub(6)).max(1));
            for (_, rect, _) in &plan.buttons {
                assert!(rect.right() <= area.right().max(rect.x), "{w}x{h}");
            }
        }
        let plan = plan(Rect::new(0, 0, 120, 40), &state);
        assert!(
            plan.lines.last().unwrap().1.contains("Copy details"),
            "{:?}",
            plan.lines.last()
        );
    }

    #[test]
    fn the_copied_details_carry_both_the_reason_and_the_failure() {
        let text = details(&expired("lost it", Some("could not open a browser")));
        assert!(text.contains("lost it") && text.contains("could not open a browser"));
        assert!(!details(&expired("", None)).contains("Why"));
    }

    #[test]
    fn the_waiting_popup_says_to_finish_in_the_browser() {
        let plan = plan(
            Rect::new(0, 0, 100, 30),
            &SignIn::Waiting { reason: "r".into() },
        );
        assert_eq!(plan.lines[0].1, "Signing in…");
        assert!(plan.lines.iter().any(|(_, l)| l.contains("browser")));
    }
}
