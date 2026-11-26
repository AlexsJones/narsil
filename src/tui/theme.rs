//! Colour themes. The interface is written in the terminal's sixteen named
//! colours, each used for one thing throughout: light blue for what frames
//! and heads, dark grey for what recedes, yellow for work under way, green
//! for done, red for what failed. A theme says what each of those is, and
//! what stands behind them; it is laid over the finished screen, cell by
//! cell, just before it is shown. So every part of the interface follows the
//! theme without knowing there is one.
//!
//! The `terminal` theme lays nothing over: the colours are the terminal's own.

use std::sync::RwLock;

use ratatui::buffer::Buffer;
use ratatui::style::Color;

pub type Rgb = (u8, u8, u8);

pub struct Theme {
    pub name: &'static str,
    pub about: &'static str,
    /// What everything stands on, and what plain text is written in.
    pub bg: Rgb,
    pub fg: Rgb,
    /// What recedes: hints, borders at rest, what is finished.
    pub dim: Rgb,
    /// What frames and heads: borders in use, headings, the selection.
    pub accent: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub magenta: Rgb,
    pub cyan: Rgb,
    /// The Obsidian vault's own colour.
    pub purple: Rgb,
}

const fn hex(v: u32) -> Rgb {
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// The name of the theme that lays nothing over the terminal's own colours.
pub const TERMINAL: &str = "terminal";

pub const THEMES: [Theme; 12] = [
    Theme {
        name: "gruvbox",
        about: "warm and low in contrast, on dark",
        bg: hex(0x282828),
        fg: hex(0xebdbb2),
        dim: hex(0x928374),
        accent: hex(0xfe8019),
        red: hex(0xfb4934),
        green: hex(0xb8bb26),
        yellow: hex(0xfabd2f),
        magenta: hex(0xd3869b),
        cyan: hex(0x8ec07c),
        purple: hex(0xb16286),
    },
    Theme {
        name: "gruvbox-light",
        about: "the same warmth, on cream",
        bg: hex(0xfbf1c7),
        fg: hex(0x3c3836),
        dim: hex(0x928374),
        accent: hex(0xaf3a03),
        red: hex(0x9d0006),
        green: hex(0x79740e),
        yellow: hex(0xb57614),
        magenta: hex(0x8f3f71),
        cyan: hex(0x427b58),
        purple: hex(0x8f3f71),
    },
    Theme {
        name: "solarized",
        about: "the measured sixteen, on deep teal",
        bg: hex(0x002b36),
        fg: hex(0x93a1a1),
        dim: hex(0x586e75),
        accent: hex(0x268bd2),
        red: hex(0xdc322f),
        green: hex(0x859900),
        yellow: hex(0xb58900),
        magenta: hex(0xd33682),
        cyan: hex(0x2aa198),
        purple: hex(0x6c71c4),
    },
    Theme {
        name: "solarized-light",
        about: "the same sixteen, on paper",
        bg: hex(0xfdf6e3),
        fg: hex(0x586e75),
        dim: hex(0x93a1a1),
        accent: hex(0x268bd2),
        red: hex(0xdc322f),
        green: hex(0x859900),
        yellow: hex(0xb58900),
        magenta: hex(0xd33682),
        cyan: hex(0x2aa198),
        purple: hex(0x6c71c4),
    },
    Theme {
        name: "dracula",
        about: "purple and pink, on slate",
        bg: hex(0x282a36),
        fg: hex(0xf8f8f2),
        dim: hex(0x6272a4),
        accent: hex(0xbd93f9),
        red: hex(0xff5555),
        green: hex(0x50fa7b),
        yellow: hex(0xf1fa8c),
        magenta: hex(0xff79c6),
        cyan: hex(0x8be9fd),
        purple: hex(0xbd93f9),
    },
    Theme {
        name: "nord",
        about: "arctic blues, cool and quiet",
        bg: hex(0x2e3440),
        fg: hex(0xd8dee9),
        dim: hex(0x616e88),
        accent: hex(0x88c0d0),
        red: hex(0xbf616a),
        green: hex(0xa3be8c),
        yellow: hex(0xebcb8b),
        magenta: hex(0xb48ead),
        cyan: hex(0x8fbcbb),
        purple: hex(0xb48ead),
    },
    Theme {
        name: "tokyo-night",
        about: "neon on midnight blue",
        bg: hex(0x1a1b26),
        fg: hex(0xc0caf5),
        dim: hex(0x565f89),
        accent: hex(0x7aa2f7),
        red: hex(0xf7768e),
        green: hex(0x9ece6a),
        yellow: hex(0xe0af68),
        magenta: hex(0xbb9af7),
        cyan: hex(0x7dcfff),
        purple: hex(0x9d7cd8),
    },
    Theme {
        name: "catppuccin",
        about: "soft pastels, on mocha",
        bg: hex(0x1e1e2e),
        fg: hex(0xcdd6f4),
        dim: hex(0x6c7086),
        accent: hex(0x89b4fa),
        red: hex(0xf38ba8),
        green: hex(0xa6e3a1),
        yellow: hex(0xf9e2af),
        magenta: hex(0xf5c2e7),
        cyan: hex(0x94e2d5),
        purple: hex(0xcba6f7),
    },
    Theme {
        name: "monokai",
        about: "loud and bright, on olive black",
        bg: hex(0x272822),
        fg: hex(0xf8f8f2),
        dim: hex(0x75715e),
        accent: hex(0x66d9ef),
        red: hex(0xf92672),
        green: hex(0xa6e22e),
        yellow: hex(0xe6db74),
        magenta: hex(0xfd971f),
        cyan: hex(0xa1efe4),
        purple: hex(0xae81ff),
    },
    Theme {
        name: "one-dark",
        about: "even and familiar, on charcoal",
        bg: hex(0x282c34),
        fg: hex(0xabb2bf),
        dim: hex(0x5c6370),
        accent: hex(0x61afef),
        red: hex(0xe06c75),
        green: hex(0x98c379),
        yellow: hex(0xe5c07b),
        magenta: hex(0xc678dd),
        cyan: hex(0x56b6c2),
        purple: hex(0xc678dd),
    },
    Theme {
        name: "amber",
        about: "an amber monitor, 1983",
        bg: hex(0x150d00),
        fg: hex(0xffb000),
        dim: hex(0x9a6a0a),
        accent: hex(0xffd270),
        red: hex(0xff6a3d),
        green: hex(0xffc94a),
        yellow: hex(0xffe08a),
        magenta: hex(0xff9a5a),
        cyan: hex(0xffd9a0),
        purple: hex(0xffb86b),
    },
    Theme {
        name: "phosphor",
        about: "a green phosphor terminal",
        bg: hex(0x020a04),
        fg: hex(0x33ff66),
        dim: hex(0x1f9940),
        accent: hex(0x9dffb8),
        red: hex(0xff6b6b),
        green: hex(0x33ff66),
        yellow: hex(0xc8ff6b),
        magenta: hex(0x7dffcf),
        cyan: hex(0x66ffd9),
        purple: hex(0x9dffb0),
    },
];

/// Every theme by name, the terminal's own first.
pub fn names() -> Vec<&'static str> {
    std::iter::once(TERMINAL).chain(THEMES.iter().map(|t| t.name)).collect()
}

pub fn find(name: &str) -> Option<&'static Theme> {
    let name = name.trim().to_lowercase();
    THEMES.iter().find(|t| t.name == name)
}

/// The theme in force, by its place in `THEMES`; None is the terminal's own colours.
static CURRENT: RwLock<Option<usize>> = RwLock::new(None);

/// Put a theme in force. An unknown name leaves the terminal's own colours, and says so.
pub fn set(name: &str) -> bool {
    let name = name.trim().to_lowercase();
    let at = THEMES.iter().position(|t| t.name == name);
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = at;
    at.is_some() || name == TERMINAL || name.is_empty()
}

/// The name of the theme in force.
pub fn current() -> &'static str {
    CURRENT.read().unwrap_or_else(|e| e.into_inner()).map(|at| THEMES[at].name).unwrap_or(TERMINAL)
}

/// A colour as this terminal can show it: as it is where there are sixteen
/// million to choose from, and the nearest of 256 where there are not.
pub fn shown((r, g, b): Rgb) -> Color {
    static FULL: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit")));
    if *FULL {
        return Color::Rgb(r, g, b);
    }
    // The 6×6×6 cube of the 256-colour palette, whose steps are 0, 95, 135, 175, 215, 255.
    let step = |v: u8| -> u8 {
        match v {
            0..=47 => 0,
            48..=114 => 1,
            _ => (v - 35) / 40,
        }
    };
    Color::Indexed(16 + 36 * step(r) + 6 * step(g) + step(b))
}

impl Theme {
    /// What one of the interface's named colours is in this theme. `ground`
    /// is true for what stands behind text.
    fn colour(&self, named: Color, ground: bool) -> Color {
        shown(match named {
            Color::Reset if ground => self.bg,
            Color::Reset | Color::White | Color::Gray => self.fg,
            // Text on a selection: the colour everything stands on, so that it reads on the accent.
            Color::Black => self.bg,
            Color::DarkGray => self.dim,
            Color::LightBlue | Color::Blue => self.accent,
            Color::Red | Color::LightRed => self.red,
            Color::Green | Color::LightGreen => self.green,
            Color::Yellow | Color::LightYellow => self.yellow,
            Color::Magenta | Color::LightMagenta => self.magenta,
            Color::Cyan | Color::LightCyan => self.cyan,
            // The Obsidian vault's purple.
            Color::Indexed(141) => self.purple,
            // A colour given outright (a theme's own swatch) is what it says it is.
            other => return other,
        })
    }
}

/// Lay the theme in force over a finished screen.
pub fn paint(screen: &mut Buffer) {
    let Some(at) = *CURRENT.read().unwrap_or_else(|e| e.into_inner()) else { return };
    let theme = &THEMES[at];
    for cell in screen.content.iter_mut() {
        cell.fg = theme.colour(cell.fg, false);
        cell.bg = theme.colour(cell.bg, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    #[test]
    fn every_theme_is_complete_and_can_be_read() {
        let names = names();
        assert_eq!(names[0], TERMINAL);
        assert!(names.contains(&"gruvbox") && names.len() == THEMES.len() + 1);
        for (i, theme) in THEMES.iter().enumerate() {
            assert!(theme.name == theme.name.to_lowercase() && !theme.about.is_empty(), "{}", theme.name);
            assert!(!THEMES[..i].iter().any(|t| t.name == theme.name), "{} is listed twice", theme.name);
            // Text has to stand out from what it is on: a crude measure of lightness is enough to catch a slip.
            let light = |(r, g, b): Rgb| (r as i32 * 3 + g as i32 * 6 + b as i32) / 10;
            let ground = light(theme.bg);
            for (what, colour, least) in [
                ("text", theme.fg, 90),
                ("accent", theme.accent, 60),
                ("dim", theme.dim, 35),
                ("red", theme.red, 40),
                ("green", theme.green, 40),
                ("yellow", theme.yellow, 40),
                ("magenta", theme.magenta, 40),
                ("cyan", theme.cyan, 40),
                ("purple", theme.purple, 35),
            ] {
                assert!((light(colour) - ground).abs() >= least, "{}: {what} is too close to its background", theme.name);
            }
        }
        assert_eq!(find("Gruvbox").map(|t| t.name), Some("gruvbox"));
        assert!(find("terminal").is_none() && find("no-such").is_none());
    }

    #[test]
    fn a_theme_is_laid_over_the_named_colours_and_leaves_the_rest() {
        let mut screen = Buffer::empty(Rect::new(0, 0, 4, 1));
        screen[(0, 0)].set_style(Style::new().fg(Color::LightBlue));
        screen[(1, 0)].set_style(Style::new().fg(Color::Black).bg(Color::LightBlue));
        screen[(2, 0)].set_style(Style::new().fg(Color::Rgb(1, 2, 3)));
        let before = screen.clone();

        // The terminal's own colours: nothing is touched.
        assert!(set(TERMINAL) && current() == TERMINAL);
        paint(&mut screen);
        assert_eq!(screen, before);

        assert!(set("gruvbox") && current() == "gruvbox");
        paint(&mut screen);
        let g = find("gruvbox").unwrap();
        assert_eq!((screen[(0, 0)].fg, screen[(0, 0)].bg), (shown(g.accent), shown(g.bg)), "an accent, on the theme's ground");
        assert_eq!((screen[(1, 0)].fg, screen[(1, 0)].bg), (shown(g.bg), shown(g.accent)), "a selection reads the other way round");
        assert_eq!(screen[(2, 0)].fg, Color::Rgb(1, 2, 3), "a colour given outright is kept");
        assert_eq!((screen[(3, 0)].fg, screen[(3, 0)].bg), (shown(g.fg), shown(g.bg)), "plain text and bare ground take the theme's");

        // A name nobody knows falls back to the terminal's own, and says it was not known.
        assert!(!set("no-such-theme") && current() == TERMINAL);
        assert!(set(""));
    }
}
