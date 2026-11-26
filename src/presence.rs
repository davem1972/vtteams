//! How a person's availability is drawn: a coloured circle in the way Teams does it (green available, red busy, a
//! barred red do-not-disturb, yellow away or back soon, grey offline, purple out of office), as an emoji or, for a
//! font or terminal that draws those badly, as a one-column coloured dot. Pure, so it is tested without a terminal.

use crate::theme::{ACCENT, DIM, ERR, OK, WARN};
use ratatui::style::Color;
use teams_client::{Availability, Presence};

/// How availability is shown (the `presence` setting).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PresenceStyle {
    /// A coloured emoji, two columns wide.
    #[default]
    Emoji,
    /// A coloured dot, one column wide.
    Dot,
    /// Nothing; availability is not even asked for.
    Off,
}

impl PresenceStyle {
    /// The style a setting's value names.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "emoji" | "on" => Some(Self::Emoji),
            "dot" | "dots" => Some(Self::Dot),
            "off" | "none" => Some(Self::Off),
            _ => None,
        }
    }

    /// The word for it in the settings.
    pub fn label(self) -> &'static str {
        match self {
            Self::Emoji => "emoji",
            Self::Dot => "dot",
            Self::Off => "off",
        }
    }

    /// How many terminal columns a mark takes (none when off).
    pub fn width(self) -> usize {
        match self {
            Self::Emoji => 2,
            Self::Dot => 1,
            Self::Off => 0,
        }
    }
}

/// A mark to draw, and the colour to draw it in (an emoji brings its own).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mark {
    pub glyph: &'static str,
    pub color: Color,
}

/// What a presence looks like, before it is turned into a glyph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Look {
    Green,
    Red,
    Barred,
    Yellow,
    Grey,
    Purple,
}

fn look(p: &Presence) -> Option<Look> {
    use Availability::{
        Available, AvailableIdle, Away, BeRightBack, Busy, BusyIdle, DoNotDisturb, Offline, Unknown,
    };
    Some(match p.availability {
        // Someone who is actually working is shown as that, whatever their calendar says; otherwise being out of
        // office is what is worth knowing.
        AvailableIdle | Away | BeRightBack | Offline | Unknown if p.out_of_office => Look::Purple,
        Available => Look::Green,
        Busy | BusyIdle => Look::Red,
        DoNotDisturb => Look::Barred,
        // Teams draws someone who is signed in but idle as away, the same yellow as one who set it themselves.
        Away | AvailableIdle | BeRightBack => Look::Yellow,
        Offline => Look::Grey,
        // Not knowing is not "offline": nothing is drawn.
        Unknown => return None,
        // `Availability` is `#[non_exhaustive]`: one a later version adds is not drawn until it has a look.
        _ => return None,
    })
}

/// The mark for `p` in `style`, or `None` when nothing should be drawn (presence off, or availability unknown).
pub fn mark(style: PresenceStyle, p: &Presence) -> Option<Mark> {
    let look = look(p)?;
    let color = match look {
        Look::Green => OK,
        Look::Red | Look::Barred => ERR,
        Look::Yellow => WARN,
        Look::Grey => DIM,
        Look::Purple => ACCENT,
    };
    let glyph = match (style, look) {
        (PresenceStyle::Off, _) => return None,
        (PresenceStyle::Emoji, Look::Green) => "🟢",
        (PresenceStyle::Emoji, Look::Red) => "🔴",
        (PresenceStyle::Emoji, Look::Barred) => "⛔",
        (PresenceStyle::Emoji, Look::Yellow) => "🟡",
        (PresenceStyle::Emoji, Look::Grey) => "⚪",
        (PresenceStyle::Emoji, Look::Purple) => "🟣",
        (PresenceStyle::Dot, Look::Barred) => "⊘",
        (PresenceStyle::Dot, _) => "●",
    };
    Some(Mark { glyph, color })
}

/// What to say of `p` in words: "Busy", "Offline · out of office".
pub fn describe(p: &Presence) -> String {
    let name = p.availability.name();
    if p.out_of_office {
        format!("{name} · out of office")
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    fn p(availability: Availability, out_of_office: bool) -> Presence {
        Presence::new(availability, out_of_office)
    }

    #[test]
    fn each_status_has_the_mark_teams_gives_it() {
        use Availability::*;
        let emoji = |a, ooo| mark(PresenceStyle::Emoji, &p(a, ooo)).map(|m| m.glyph);
        assert_eq!(emoji(Available, false), Some("🟢"));
        assert_eq!(
            emoji(AvailableIdle, false),
            Some("🟡"),
            "idle is away, as Teams draws it"
        );
        assert_eq!(emoji(Busy, false), Some("🔴"));
        assert_eq!(emoji(BusyIdle, false), Some("🔴"));
        assert_eq!(emoji(DoNotDisturb, false), Some("⛔"));
        assert_eq!(emoji(Away, false), Some("🟡"));
        assert_eq!(emoji(BeRightBack, false), Some("🟡"));
        assert_eq!(emoji(Offline, false), Some("⚪"));
        assert_eq!(emoji(Unknown, false), None, "not knowing is not offline");
    }

    #[test]
    fn out_of_office_shows_purple_unless_the_person_is_actively_in_something() {
        use Availability::*;
        let glyph = |a| mark(PresenceStyle::Emoji, &p(a, true)).map(|m| m.glyph);
        for away in [Away, BeRightBack, Offline, AvailableIdle, Unknown] {
            assert_eq!(glyph(away), Some("🟣"), "{away:?}");
        }
        for busy in [Available, Busy, DoNotDisturb] {
            assert_ne!(glyph(busy), Some("🟣"), "{busy:?}");
        }
    }

    #[test]
    fn a_mark_takes_exactly_the_columns_the_layout_reserves_for_it() {
        use Availability::*;
        let all = [
            Available,
            AvailableIdle,
            Away,
            BeRightBack,
            Busy,
            BusyIdle,
            DoNotDisturb,
            Offline,
        ];
        for style in [PresenceStyle::Emoji, PresenceStyle::Dot] {
            for a in all {
                for ooo in [false, true] {
                    let m = mark(style, &p(a, ooo)).expect("a known availability is drawn");
                    assert_eq!(m.glyph.width(), style.width(), "{style:?} {a:?} {ooo}");
                }
            }
        }
        assert_eq!(PresenceStyle::Off.width(), 0);
        assert!(mark(PresenceStyle::Off, &p(Available, false)).is_none());
    }

    #[test]
    fn dots_tell_do_not_disturb_apart_by_shape_and_the_rest_by_colour() {
        use Availability::*;
        let dot = |a| mark(PresenceStyle::Dot, &p(a, false)).unwrap();
        assert_eq!(dot(Available).glyph, "●");
        assert_ne!(dot(DoNotDisturb).glyph, dot(Busy).glyph);
        let colours = [Available, Busy, Away, Offline].map(|a| dot(a).color);
        for (i, a) in colours.iter().enumerate() {
            for b in &colours[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn words_and_settings() {
        assert_eq!(
            describe(&p(Availability::BeRightBack, false)),
            "Be right back"
        );
        assert_eq!(
            describe(&p(Availability::Offline, true)),
            "Offline · out of office"
        );
        assert_eq!(describe(&p(Availability::AvailableIdle, false)), "Away");
        assert_eq!(describe(&p(Availability::BusyIdle, false)), "Busy");
        for (text, style) in [
            ("emoji", PresenceStyle::Emoji),
            ("DOT", PresenceStyle::Dot),
            (" off ", PresenceStyle::Off),
            ("on", PresenceStyle::Emoji),
        ] {
            assert_eq!(PresenceStyle::parse(text), Some(style), "{text}");
        }
        assert_eq!(PresenceStyle::parse("sometimes"), None);
        assert_eq!(PresenceStyle::default(), PresenceStyle::Emoji);
    }
}
