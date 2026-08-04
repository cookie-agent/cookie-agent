//! Semantic terminal themes and capability-aware color quantization.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ThemeKind {
    Default,
    Mono,
    HighContrast,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ColorLevel {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ThemeKey {
    pub kind: ThemeKind,
    pub colors: ColorLevel,
}

#[derive(Clone, Debug)]
pub struct Theme {
    key: ThemeKey,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new(ThemeKind::Default, ColorLevel::TrueColor)
    }
}

impl Theme {
    pub fn from_env() -> Self {
        let requested = std::env::var("COOKIE_THEME").unwrap_or_default();
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let term = std::env::var("TERM").unwrap_or_default();
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        Self::from_environment(&requested, no_color, &term, &colorterm)
    }

    /// Apply a theme kind chosen by the TUI config file while still honoring
    /// `NO_COLOR`/`TERM=dumb` and detected terminal color capability.
    pub fn with_kind_from_env(kind: ThemeKind) -> Self {
        let requested = match kind {
            ThemeKind::Default => "default",
            ThemeKind::Mono => "mono",
            ThemeKind::HighContrast => "high-contrast",
        };
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let term = std::env::var("TERM").unwrap_or_default();
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        Self::from_environment(requested, no_color, &term, &colorterm)
    }

    pub fn from_environment(requested: &str, no_color: bool, term: &str, colorterm: &str) -> Self {
        let requested_kind = match requested.to_ascii_lowercase().as_str() {
            "mono" | "monochrome" => ThemeKind::Mono,
            "high-contrast" | "high_contrast" | "contrast" => ThemeKind::HighContrast,
            _ => ThemeKind::Default,
        };
        let colors =
            if requested_kind == ThemeKind::Mono || no_color || term.eq_ignore_ascii_case("dumb") {
                ColorLevel::None
            } else if requested_kind == ThemeKind::HighContrast {
                ColorLevel::Ansi16
            } else if colorterm.eq_ignore_ascii_case("truecolor")
                || colorterm.eq_ignore_ascii_case("24bit")
            {
                ColorLevel::TrueColor
            } else if term.contains("256color") {
                ColorLevel::Ansi256
            } else {
                ColorLevel::Ansi16
            };
        let kind = if colors == ColorLevel::None {
            ThemeKind::Mono
        } else {
            requested_kind
        };
        Self::new(kind, colors)
    }

    pub const fn new(kind: ThemeKind, colors: ColorLevel) -> Self {
        let colors = match (kind, colors) {
            (ThemeKind::Mono, _) | (ThemeKind::HighContrast, ColorLevel::None) => ColorLevel::None,
            (ThemeKind::HighContrast, _) => ColorLevel::Ansi16,
            (ThemeKind::Default, colors) => colors,
        };
        Self {
            key: ThemeKey { kind, colors },
        }
    }

    pub const fn key(&self) -> ThemeKey {
        self.key
    }

    pub fn body(&self) -> Style {
        Style::default()
    }

    pub fn muted(&self) -> Style {
        self.semantic(
            (112, 122, 136),
            Color::DarkGray,
            Color::White,
            Modifier::DIM,
        )
    }

    pub fn user(&self) -> Style {
        self.semantic(
            (64, 196, 255),
            Color::Cyan,
            Color::LightCyan,
            Modifier::BOLD,
        )
    }

    pub fn assistant(&self) -> Style {
        self.semantic((96, 220, 160), Color::Green, Color::White, Modifier::BOLD)
    }

    pub fn thinking(&self) -> Style {
        self.semantic(
            (170, 150, 220),
            Color::Magenta,
            Color::LightMagenta,
            Modifier::ITALIC,
        )
    }

    /// Selected thinking-toggle row: same hue with an added underline so a
    /// selected block is distinct without any extra glyph — important for
    /// mono/no-color terminals where color is unavailable.
    pub fn thinking_selected(&self) -> Style {
        self.thinking().add_modifier(Modifier::UNDERLINED)
    }

    pub fn tool(&self) -> Style {
        self.semantic(
            (245, 190, 80),
            Color::Yellow,
            Color::LightYellow,
            Modifier::BOLD,
        )
    }

    pub fn tool_running(&self) -> Style {
        self.tool()
    }

    pub fn tool_success(&self) -> Style {
        self.semantic(
            (96, 220, 160),
            Color::Green,
            Color::LightGreen,
            Modifier::BOLD,
        )
    }

    pub fn tool_failure(&self) -> Style {
        self.error()
    }

    /// Warning style — visually distinct from error styling (never red).
    pub fn warning(&self) -> Style {
        self.semantic(
            (255, 205, 90),
            Color::Yellow,
            Color::LightYellow,
            Modifier::BOLD,
        )
    }

    pub fn error(&self) -> Style {
        self.semantic((255, 100, 100), Color::Red, Color::LightRed, Modifier::BOLD)
    }

    pub fn internal(&self) -> Style {
        self.semantic((150, 160, 175), Color::Gray, Color::Gray, Modifier::DIM)
    }

    pub fn heading(&self) -> Style {
        let style = self.semantic((255, 215, 110), Color::Yellow, Color::White, Modifier::BOLD);
        if self.key.kind == ThemeKind::HighContrast && self.key.colors != ColorLevel::None {
            style.add_modifier(Modifier::UNDERLINED)
        } else {
            style
        }
    }

    pub fn link(&self) -> Style {
        self.semantic(
            (90, 170, 255),
            Color::Blue,
            Color::LightCyan,
            Modifier::UNDERLINED | Modifier::BOLD,
        )
    }

    /// Inline code in assistant Markdown: a distinct semantic background
    /// with a readable foreground, never color-only — the source backticks
    /// stay visible, and the bold modifier carries the distinction in mono
    /// terminals.
    pub fn inline_code(&self) -> Style {
        let foreground =
            self.semantic((255, 175, 100), Color::Yellow, Color::Black, Modifier::BOLD);
        let background = match self.key.colors {
            ColorLevel::None => None,
            _ if self.key.kind == ThemeKind::HighContrast => Some(Color::LightYellow),
            ColorLevel::Ansi16 => Some(Color::Black),
            ColorLevel::Ansi256 | ColorLevel::TrueColor => self.quantize_rgb(48, 52, 70),
        };
        background.map_or(foreground, |background| foreground.bg(background))
    }

    pub fn code_border(&self) -> Style {
        self.semantic((120, 150, 180), Color::Blue, Color::White, Modifier::DIM)
    }

    pub fn quote(&self) -> Style {
        self.semantic(
            (175, 185, 200),
            Color::Gray,
            Color::LightMagenta,
            Modifier::ITALIC,
        )
    }

    pub fn input_border(&self, focused: bool) -> Style {
        if focused { self.user() } else { self.muted() }
    }

    pub fn quantize_rgb(&self, red: u8, green: u8, blue: u8) -> Option<Color> {
        match self.key.colors {
            ColorLevel::None => None,
            _ if self.key.kind == ThemeKind::HighContrast => {
                Some(nearest_high_contrast(red, green, blue))
            }
            ColorLevel::TrueColor => Some(Color::Rgb(red, green, blue)),
            ColorLevel::Ansi256 => Some(Color::Indexed(rgb_to_ansi256(red, green, blue))),
            ColorLevel::Ansi16 => Some(nearest_ansi16(red, green, blue)),
        }
    }

    fn semantic(
        &self,
        rgb: (u8, u8, u8),
        ansi: Color,
        high_contrast: Color,
        modifier: Modifier,
    ) -> Style {
        let modifier = match self.key.kind {
            ThemeKind::HighContrast => modifier | Modifier::BOLD,
            ThemeKind::Default | ThemeKind::Mono => modifier,
        };
        let color = match self.key.colors {
            ColorLevel::None => None,
            _ if self.key.kind == ThemeKind::HighContrast => Some(high_contrast),
            ColorLevel::Ansi16 => Some(ansi),
            ColorLevel::Ansi256 | ColorLevel::TrueColor => self.quantize_rgb(rgb.0, rgb.1, rgb.2),
        };
        color.map_or_else(
            || Style::default().add_modifier(modifier),
            |color| Style::default().fg(color).add_modifier(modifier),
        )
    }
}

fn nearest_high_contrast(red: u8, green: u8, blue: u8) -> Color {
    const COLORS: [(Color, (u8, u8, u8)); 7] = [
        (Color::LightRed, (255, 0, 0)),
        (Color::LightGreen, (0, 255, 0)),
        (Color::LightYellow, (255, 255, 0)),
        (Color::LightBlue, (0, 0, 255)),
        (Color::LightMagenta, (255, 0, 255)),
        (Color::LightCyan, (0, 255, 255)),
        (Color::White, (255, 255, 255)),
    ];
    nearest_color(red, green, blue, &COLORS)
}

fn rgb_to_ansi256(red: u8, green: u8, blue: u8) -> u8 {
    let component = |value: u8| ((u16::from(value) * 5 + 127) / 255) as u8;
    16 + 36 * component(red) + 6 * component(green) + component(blue)
}

fn nearest_ansi16(red: u8, green: u8, blue: u8) -> Color {
    const COLORS: [(Color, (u8, u8, u8)); 16] = [
        (Color::Black, (0, 0, 0)),
        (Color::Red, (128, 0, 0)),
        (Color::Green, (0, 128, 0)),
        (Color::Yellow, (128, 128, 0)),
        (Color::Blue, (0, 0, 128)),
        (Color::Magenta, (128, 0, 128)),
        (Color::Cyan, (0, 128, 128)),
        (Color::Gray, (192, 192, 192)),
        (Color::DarkGray, (128, 128, 128)),
        (Color::LightRed, (255, 0, 0)),
        (Color::LightGreen, (0, 255, 0)),
        (Color::LightYellow, (255, 255, 0)),
        (Color::LightBlue, (0, 0, 255)),
        (Color::LightMagenta, (255, 0, 255)),
        (Color::LightCyan, (0, 255, 255)),
        (Color::White, (255, 255, 255)),
    ];
    nearest_color(red, green, blue, &COLORS)
}

fn nearest_color(red: u8, green: u8, blue: u8, colors: &[(Color, (u8, u8, u8))]) -> Color {
    colors
        .iter()
        .min_by_key(|(_, (candidate_red, candidate_green, candidate_blue))| {
            let red = i32::from(red) - i32::from(*candidate_red);
            let green = i32::from(green) - i32::from(*candidate_green);
            let blue = i32::from(blue) - i32::from(*candidate_blue);
            red * red + green * green + blue * blue
        })
        .map_or(Color::White, |(color, _)| *color)
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ratatui::style::{Color, Modifier, Style};

    use super::{ColorLevel, Theme, ThemeKind};

    #[test]
    fn environment_selects_themes_and_disables_color_safely() {
        assert_eq!(
            Theme::from_environment("high-contrast", false, "xterm-256color", "").key(),
            super::ThemeKey {
                kind: ThemeKind::HighContrast,
                colors: ColorLevel::Ansi16,
            }
        );
        for theme in [
            Theme::from_environment("mono", false, "xterm-256color", "truecolor"),
            Theme::from_environment("default", true, "xterm-256color", "truecolor"),
            Theme::from_environment("default", false, "dumb", "truecolor"),
            Theme::new(ThemeKind::Mono, ColorLevel::TrueColor),
        ] {
            assert_eq!(theme.key().kind, ThemeKind::Mono);
            assert_eq!(theme.key().colors, ColorLevel::None);
            assert!(theme.assistant().fg.is_none());
        }
    }

    #[test]
    fn colors_quantize_for_terminal_capabilities() {
        assert!(matches!(
            Theme::new(ThemeKind::Default, ColorLevel::TrueColor).quantize_rgb(12, 34, 56),
            Some(Color::Rgb(12, 34, 56))
        ));
        assert!(matches!(
            Theme::new(ThemeKind::Default, ColorLevel::Ansi256).quantize_rgb(12, 34, 56),
            Some(Color::Indexed(_))
        ));
        assert!(matches!(
            Theme::new(ThemeKind::Default, ColorLevel::Ansi16).quantize_rgb(12, 34, 56),
            Some(Color::Black | Color::Blue | Color::DarkGray)
        ));
    }

    #[test]
    fn semantic_theme_snapshot_is_deterministic_with_and_without_color() {
        fn signature(name: &str, style: Style) -> String {
            format!(
                "{name}: fg={:?} bg={:?} bold={} italic={} underline={} dim={} reverse={}",
                style.fg,
                style.bg,
                style.add_modifier.contains(Modifier::BOLD),
                style.add_modifier.contains(Modifier::ITALIC),
                style.add_modifier.contains(Modifier::UNDERLINED),
                style.add_modifier.contains(Modifier::DIM),
                style.add_modifier.contains(Modifier::REVERSED),
            )
        }

        let default = Theme::new(ThemeKind::Default, ColorLevel::TrueColor);
        let contrast = Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16);
        let mono = Theme::new(ThemeKind::Mono, ColorLevel::None);
        let snapshot = [
            signature("default.user", default.user()),
            signature("default.assistant", default.assistant()),
            signature("default.tool_running", default.tool_running()),
            signature("default.tool_success", default.tool_success()),
            signature("default.tool_failure", default.tool_failure()),
            signature("contrast.user", contrast.user()),
            signature("contrast.assistant", contrast.assistant()),
            signature("contrast.tool_success", contrast.tool_success()),
            signature("contrast.error", contrast.error()),
            signature("default.warning", default.warning()),
            signature("contrast.warning", contrast.warning()),
            signature("mono.warning", mono.warning()),
            signature("contrast.heading", contrast.heading()),
            signature("default.inline_code", default.inline_code()),
            signature("contrast.inline_code", contrast.inline_code()),
            signature("mono.link", mono.link()),
            signature("mono.inline_code", mono.inline_code()),
        ]
        .join("\n");
        assert_snapshot!(snapshot, @r#"
default.user: fg=Some(Rgb(64, 196, 255)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.assistant: fg=Some(Rgb(96, 220, 160)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.tool_running: fg=Some(Rgb(245, 190, 80)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.tool_success: fg=Some(Rgb(96, 220, 160)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.tool_failure: fg=Some(Rgb(255, 100, 100)) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.user: fg=Some(LightCyan) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.assistant: fg=Some(White) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.tool_success: fg=Some(LightGreen) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.error: fg=Some(LightRed) bg=None bold=true italic=false underline=false dim=false reverse=false
default.warning: fg=Some(Rgb(255, 205, 90)) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.warning: fg=Some(LightYellow) bg=None bold=true italic=false underline=false dim=false reverse=false
mono.warning: fg=None bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.heading: fg=Some(White) bg=None bold=true italic=false underline=true dim=false reverse=false
default.inline_code: fg=Some(Rgb(255, 175, 100)) bg=Some(Rgb(48, 52, 70)) bold=true italic=false underline=false dim=false reverse=false
contrast.inline_code: fg=Some(Black) bg=Some(LightYellow) bold=true italic=false underline=false dim=false reverse=false
mono.link: fg=None bg=None bold=true italic=false underline=true dim=false reverse=false
mono.inline_code: fg=None bg=None bold=true italic=false underline=false dim=false reverse=false
"#);
    }

    #[test]
    fn inline_code_sets_a_background_in_color_themes_only() {
        for (theme, has_background) in [
            (Theme::new(ThemeKind::Default, ColorLevel::TrueColor), true),
            (Theme::new(ThemeKind::Default, ColorLevel::Ansi256), true),
            (Theme::new(ThemeKind::Default, ColorLevel::Ansi16), true),
            (
                Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16),
                true,
            ),
            (Theme::new(ThemeKind::Mono, ColorLevel::None), false),
        ] {
            let style = theme.inline_code();
            assert_eq!(
                style.bg.is_some(),
                has_background,
                "background: {:?}",
                theme.key()
            );
            assert!(
                !style.add_modifier.contains(Modifier::REVERSED),
                "no reverse video: {:?}",
                theme.key()
            );
            // Distinction is never color-only: a bold modifier remains even
            // when color is unavailable.
            assert!(
                style.add_modifier.contains(Modifier::BOLD),
                "bold distinction without color: {:?}",
                theme.key()
            );
        }
    }
}
