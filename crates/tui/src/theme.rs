//! Semantic terminal themes and capability-aware color quantization.
//!
//! The default theme is a warm, light "cookie" palette: one cream surface
//! across the whole frame, espresso-brown text, and caramel/cinnamon/sage
//! accents. Every foreground is chosen for WCAG-AA-or-better contrast
//! against the cream surface; state is never conveyed by color alone
//! (bold/italic/underline and text markers accompany every semantic color).
//! `Mono` drops all color, `HighContrast` keeps bright ANSI colors on the
//! terminal's own background.

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

/// The semantic color direction of an approval decision button.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DecisionTone {
    Allow,
    Deny,
    Neutral,
}

// --- The bakery palette (true color) ---------------------------------------
//
// Surfaces: a single cream base, parchment code blocks, toasted selection,
// glaze hover, mustard borders. Text: espresso body with cocoa and latte
// derivatives. Accents: honey focus, caramel, sage, cinnamon, cranberry,
// maple, slate, plum, terracotta.

const CREAM: (u8, u8, u8) = (0xFB, 0xF4, 0xE6);
const PARCHMENT: (u8, u8, u8) = (0xF2, 0xE5, 0xCC);
const TOASTED: (u8, u8, u8) = (0xEA, 0xD7, 0xB4);
const GLAZE: (u8, u8, u8) = (0xF0, 0xDF, 0xC0);
const CRUST: (u8, u8, u8) = (0xC9, 0xAE, 0x85);
// Pane chrome (conversation, tree, pickers, unfocused input) sits one step
// darker and yellower than the crust selection wash: a muted mustard that
// reads clearly against the cream surface without turning reddish.
const BORDER: (u8, u8, u8) = (0xAE, 0x8C, 0x5A);
const ESPRESSO: (u8, u8, u8) = (0x46, 0x30, 0x1F);
const COCOA: (u8, u8, u8) = (0x6E, 0x4E, 0x38);
const LATTE: (u8, u8, u8) = (0x86, 0x69, 0x4F);
const FAWN: (u8, u8, u8) = (0x93, 0x76, 0x5B);
const CARAMEL: (u8, u8, u8) = (0x9C, 0x5A, 0x10);
const SAGE: (u8, u8, u8) = (0x4E, 0x7A, 0x34);
const OLIVE: (u8, u8, u8) = (0x3F, 0x6B, 0x2A);
const CINNAMON: (u8, u8, u8) = (0xA8, 0x5B, 0x17);
const CRANBERRY: (u8, u8, u8) = (0xAE, 0x33, 0x27);
const HONEY: (u8, u8, u8) = (0x8F, 0x64, 0x08);
const MAPLE: (u8, u8, u8) = (0x8A, 0x4A, 0x0B);
const SLATE: (u8, u8, u8) = (0x3D, 0x6A, 0x8C);
const PLUM: (u8, u8, u8) = (0x8A, 0x55, 0x70);
const TAN: (u8, u8, u8) = (0x8A, 0x6B, 0x45);
const TERRACOTTA: (u8, u8, u8) = (0xA0, 0x3A, 0x20);
const QUOTE: (u8, u8, u8) = (0x84, 0x70, 0x5C);
const ALLOW_TINT: (u8, u8, u8) = (0xDE, 0xE7, 0xC6);
const DENY_TINT: (u8, u8, u8) = (0xF3, 0xD5, 0xC9);
const NEUTRAL_TINT: (u8, u8, u8) = (0xE6, 0xDC, 0xC6);

// --- xterm-256 background bands --------------------------------------------
//
// The naive RGB→cube quantizer drags every warm beige into the pink
// (255, 215, 215) cell 224, so each light band names its cube index by hand.
// The ladder steps from light to deep: cream surface, parchment code band,
// glaze hover, toasted selection.
const CREAM_256: u8 = 231; // (255, 255, 255)
const PARCHMENT_256: u8 = 230; // (255, 255, 215)
const GLAZE_256: u8 = 223; // (255, 215, 175)
const TOASTED_256: u8 = 222; // (255, 215, 135)
const CRUST_256: u8 = 180; // (215, 175, 135)
// Hand-picked border cell: the naive RGB→cube quantization of BORDER lands
// on a grayish (175, 175, 135) and loses the yellow entirely.
const BORDER_256: u8 = 137; // (175, 135, 95)

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

    /// Base surface painted beneath the whole default-theme UI: a warm cream
    /// background with an espresso foreground, so even unstyled text stays
    /// readable. Mono and high-contrast themes keep the terminal background.
    pub fn surface(&self) -> Style {
        if self.key.kind != ThemeKind::Default {
            return Style::default();
        }
        match self.key.colors {
            ColorLevel::None => Style::default(),
            ColorLevel::Ansi16 => Style::default().fg(Color::Black).bg(Color::White),
            ColorLevel::Ansi256 => {
                let mut style = Style::default().bg(Color::Indexed(CREAM_256));
                if let Some(foreground) = self.quantize_rgb(ESPRESSO.0, ESPRESSO.1, ESPRESSO.2) {
                    style = style.fg(foreground);
                }
                style
            }
            ColorLevel::TrueColor => Style::default()
                .fg(Color::Rgb(ESPRESSO.0, ESPRESSO.1, ESPRESSO.2))
                .bg(Color::Rgb(CREAM.0, CREAM.1, CREAM.2)),
        }
    }

    /// Panels (overlays, pickers, the focused input box, the bottom bar)
    /// share the one cream surface — borders, not a second fill, delineate
    /// them. Foreground is left alone so content keeps whatever the surface
    /// painted.
    pub fn panel(&self) -> Style {
        self.background_color(CREAM, Color::White, CREAM_256, None)
            .map_or_else(Style::default, |background| Style::default().bg(background))
    }

    /// Mustard border for pane chrome (conversation, tree, pickers):
    /// darker and yellower than the pale crust wash so the frame stays
    /// visible on the cream surface.
    pub fn panel_border(&self) -> Style {
        let style = self.semantic(BORDER, Color::DarkGray, Color::White, Modifier::DIM);
        if self.key.kind == ThemeKind::Default && self.key.colors == ColorLevel::Ansi256 {
            // Foregrounds normally trust the quantizer, but it grays out
            // this warm yellow — use the hand-picked cell.
            style.fg(Color::Indexed(BORDER_256))
        } else {
            style
        }
    }

    /// Keyboard-selected row in pickers, palettes, and the session tree: a
    /// toasted background with bold espresso text. Reverse video carries the
    /// selection where no color is available.
    pub fn selected(&self) -> Style {
        let style = Style::default().add_modifier(Modifier::BOLD);
        match self.key.colors {
            ColorLevel::None => Style::default().add_modifier(Modifier::REVERSED),
            _ if self.key.kind == ThemeKind::HighContrast => {
                style.fg(Color::Black).bg(Color::LightYellow)
            }
            ColorLevel::Ansi16 => style.fg(Color::Black).bg(Color::Gray),
            ColorLevel::Ansi256 => {
                let mut style = style.bg(Color::Indexed(TOASTED_256));
                if let Some(foreground) = self.quantize_rgb(ESPRESSO.0, ESPRESSO.1, ESPRESSO.2) {
                    style = style.fg(foreground);
                }
                style
            }
            ColorLevel::TrueColor => style
                .fg(Color::Rgb(ESPRESSO.0, ESPRESSO.1, ESPRESSO.2))
                .bg(Color::Rgb(TOASTED.0, TOASTED.1, TOASTED.2)),
        }
    }

    /// Mouse-drag text selection: a deeper crust wash than the keyboard
    /// `selected` row and never bold, so the two selection kinds never read
    /// alike. Where subtle color exists (ANSI-256, true color) only the
    /// background is set — patched cells keep their foregrounds, so
    /// selected code keeps its syntax colors. ANSI-16 and high-contrast
    /// targets are the one documented exception: their text is always a
    /// bright color, which a light-cyan wash would swallow, so the pair is
    /// pinned to black-on-light-cyan. Cruder targets add bold on top of
    /// reverse video to stay distinct.
    pub fn text_selection(&self) -> Style {
        match self.key.colors {
            ColorLevel::None => Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            _ if self.key.kind == ThemeKind::HighContrast => {
                Style::default().fg(Color::Black).bg(Color::LightCyan)
            }
            ColorLevel::Ansi16 => Style::default().fg(Color::Black).bg(Color::LightCyan),
            ColorLevel::Ansi256 => Style::default().bg(Color::Indexed(CRUST_256)),
            ColorLevel::TrueColor => Style::default().bg(Color::Rgb(CRUST.0, CRUST.1, CRUST.2)),
        }
    }

    /// Hover affordance patched over interactive text cells: a quiet glaze
    /// background — one calm step deeper than the cream surface, never a
    /// reddish tint. Cruder targets (mono, ANSI-16, high contrast) underline
    /// as well, so the affordance never relies on a subtle background shift
    /// alone. Existing foreground colors are preserved by the patch.
    pub fn hover(&self) -> Style {
        let quiet_background_only = matches!(
            (self.key.kind, self.key.colors),
            (
                ThemeKind::Default,
                ColorLevel::Ansi256 | ColorLevel::TrueColor
            )
        );
        let style = if quiet_background_only {
            Style::default()
        } else {
            Style::default().add_modifier(Modifier::UNDERLINED)
        };
        self.background_color(GLAZE, Color::Gray, GLAZE_256, Some(Color::DarkGray))
            .map_or(style, |background| style.bg(background))
    }

    /// Background-only hover fill for approval buttons and other glyph
    /// cells where an underline would not read.
    pub fn hover_fill(&self) -> Style {
        self.background_color(GLAZE, Color::Gray, GLAZE_256, Some(Color::DarkGray))
            .map_or_else(Style::default, |background| Style::default().bg(background))
    }

    /// Subtle parchment band behind fenced code blocks. Only the default
    /// theme paints one; mono and high-contrast keep blocks flat.
    pub fn code_background(&self) -> Option<Color> {
        if self.key.kind != ThemeKind::Default {
            return None;
        }
        self.background_color(PARCHMENT, Color::Gray, PARCHMENT_256, None)
    }

    /// One approval decision button. `active` (hover) fills the button with
    /// a decision-tinted background; the border/label color and the button
    /// glyphs carry the meaning, never color alone.
    pub fn decision(&self, tone: DecisionTone, active: bool) -> Style {
        if self.key.colors == ColorLevel::None {
            let mut style = Style::default().add_modifier(Modifier::BOLD);
            if active {
                style = style.add_modifier(Modifier::REVERSED);
            }
            return style;
        }
        if self.key.kind == ThemeKind::HighContrast {
            let foreground = match tone {
                DecisionTone::Allow => Color::LightGreen,
                DecisionTone::Deny => Color::LightRed,
                DecisionTone::Neutral => Color::White,
            };
            let mut style = Style::default().fg(foreground).add_modifier(Modifier::BOLD);
            if active {
                style = style.add_modifier(Modifier::REVERSED);
            }
            return style;
        }
        if self.key.colors == ColorLevel::Ansi16 {
            if active {
                let background = match tone {
                    DecisionTone::Allow => Color::LightGreen,
                    DecisionTone::Deny => Color::LightRed,
                    DecisionTone::Neutral => Color::Gray,
                };
                return Style::default()
                    .fg(Color::Black)
                    .bg(background)
                    .add_modifier(Modifier::BOLD);
            }
            let foreground = match tone {
                DecisionTone::Allow => Color::Green,
                DecisionTone::Deny => Color::Red,
                DecisionTone::Neutral => Color::Cyan,
            };
            return Style::default().fg(foreground).add_modifier(Modifier::BOLD);
        }
        let (foreground, tint) = match tone {
            DecisionTone::Allow => (OLIVE, ALLOW_TINT),
            DecisionTone::Deny => (CRANBERRY, DENY_TINT),
            DecisionTone::Neutral => (COCOA, NEUTRAL_TINT),
        };
        let mut style = Style::default().add_modifier(Modifier::BOLD);
        if let Some(foreground) = self.quantize_rgb(foreground.0, foreground.1, foreground.2) {
            style = style.fg(foreground);
        }
        if active && let Some(background) = self.quantize_rgb(tint.0, tint.1, tint.2) {
            style = style.bg(background);
        }
        style
    }

    pub fn body(&self) -> Style {
        Style::default()
    }

    pub fn muted(&self) -> Style {
        self.semantic(LATTE, Color::DarkGray, Color::White, Modifier::DIM)
    }

    pub fn user(&self) -> Style {
        self.semantic(CARAMEL, Color::Yellow, Color::LightCyan, Modifier::BOLD)
    }

    pub fn assistant(&self) -> Style {
        self.semantic(SAGE, Color::Green, Color::White, Modifier::BOLD)
    }

    pub fn thinking(&self) -> Style {
        self.semantic(PLUM, Color::Magenta, Color::LightMagenta, Modifier::ITALIC)
    }

    pub fn tool(&self) -> Style {
        self.semantic(CINNAMON, Color::Yellow, Color::LightYellow, Modifier::BOLD)
    }

    pub fn tool_running(&self) -> Style {
        self.tool()
    }

    pub fn tool_success(&self) -> Style {
        self.semantic(OLIVE, Color::Green, Color::LightGreen, Modifier::BOLD)
    }

    pub fn tool_failure(&self) -> Style {
        self.error()
    }

    /// Warning style — visually distinct from error styling (never red).
    pub fn warning(&self) -> Style {
        self.semantic(HONEY, Color::Yellow, Color::LightYellow, Modifier::BOLD)
    }

    pub fn error(&self) -> Style {
        self.semantic(CRANBERRY, Color::Red, Color::LightRed, Modifier::BOLD)
    }

    pub fn internal(&self) -> Style {
        self.semantic(FAWN, Color::Gray, Color::Gray, Modifier::DIM)
    }

    pub fn heading(&self) -> Style {
        let style = self.semantic(MAPLE, Color::Yellow, Color::White, Modifier::BOLD);
        if self.key.kind == ThemeKind::HighContrast && self.key.colors != ColorLevel::None {
            style.add_modifier(Modifier::UNDERLINED)
        } else {
            style
        }
    }

    pub fn link(&self) -> Style {
        self.semantic(
            SLATE,
            Color::Blue,
            Color::LightCyan,
            Modifier::UNDERLINED | Modifier::BOLD,
        )
    }

    /// Inline code in assistant Markdown: a distinct warm terracotta
    /// foreground, never a background in the default theme — the source
    /// backticks stay visible, and the bold modifier carries the distinction
    /// in mono terminals. High contrast keeps its inverse-video chip.
    pub fn inline_code(&self) -> Style {
        let foreground = self.semantic(TERRACOTTA, Color::Red, Color::Black, Modifier::BOLD);
        let background = match self.key.colors {
            ColorLevel::None => None,
            _ if self.key.kind == ThemeKind::HighContrast => Some(Color::LightYellow),
            ColorLevel::Ansi16 | ColorLevel::Ansi256 | ColorLevel::TrueColor => None,
        };
        background.map_or(foreground, |background| foreground.bg(background))
    }

    pub fn code_border(&self) -> Style {
        self.semantic(TAN, Color::Yellow, Color::White, Modifier::DIM)
    }

    pub fn quote(&self) -> Style {
        self.semantic(QUOTE, Color::Gray, Color::LightMagenta, Modifier::ITALIC)
    }

    pub fn input_border(&self, focused: bool) -> Style {
        if focused {
            // The theme's highlight is honey yellow, not the reddish caramel
            // of user identity — focus reads as warmth, not as an error-adjacent red.
            self.semantic(HONEY, Color::Yellow, Color::LightYellow, Modifier::BOLD)
        } else {
            self.panel_border()
        }
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

    /// A background color honoring kind/level fallbacks, with hand-picked
    /// ANSI-16 choices that stay readable on a light surface and a
    /// hand-picked xterm-256 cube index — the computed quantization drags
    /// warm beiges into pink cells, so light bands never trust it.
    fn background_color(
        &self,
        rgb: (u8, u8, u8),
        ansi16: Color,
        ansi256: u8,
        high_contrast: Option<Color>,
    ) -> Option<Color> {
        match self.key.colors {
            ColorLevel::None => None,
            _ if self.key.kind == ThemeKind::HighContrast => high_contrast,
            ColorLevel::Ansi16 => Some(ansi16),
            ColorLevel::Ansi256 => Some(Color::Indexed(ansi256)),
            ColorLevel::TrueColor => Some(Color::Rgb(rgb.0, rgb.1, rgb.2)),
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

    use super::{ColorLevel, DecisionTone, Theme, ThemeKind};

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
            signature("default.surface", default.surface()),
            signature("default.panel", default.panel()),
            signature("default.user", default.user()),
            signature("default.assistant", default.assistant()),
            signature("default.tool_running", default.tool_running()),
            signature("default.tool_success", default.tool_success()),
            signature("default.tool_failure", default.tool_failure()),
            signature("default.selected", default.selected()),
            signature("default.hover", default.hover()),
            signature(
                "default.decision.allow",
                default.decision(DecisionTone::Allow, false),
            ),
            signature(
                "default.decision.deny.active",
                default.decision(DecisionTone::Deny, true),
            ),
            signature("contrast.user", contrast.user()),
            signature("contrast.assistant", contrast.assistant()),
            signature("contrast.tool_success", contrast.tool_success()),
            signature("contrast.error", contrast.error()),
            signature("contrast.selected", contrast.selected()),
            signature("default.warning", default.warning()),
            signature("contrast.warning", contrast.warning()),
            signature("mono.warning", mono.warning()),
            signature("contrast.heading", contrast.heading()),
            signature("default.inline_code", default.inline_code()),
            signature("contrast.inline_code", contrast.inline_code()),
            signature("mono.link", mono.link()),
            signature("mono.inline_code", mono.inline_code()),
            signature("mono.selected", mono.selected()),
            signature("mono.hover", mono.hover()),
        ]
        .join("\n");
        assert_snapshot!(snapshot, @r#"
default.surface: fg=Some(Rgb(70, 48, 31)) bg=Some(Rgb(251, 244, 230)) bold=false italic=false underline=false dim=false reverse=false
default.panel: fg=None bg=Some(Rgb(251, 244, 230)) bold=false italic=false underline=false dim=false reverse=false
default.user: fg=Some(Rgb(156, 90, 16)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.assistant: fg=Some(Rgb(78, 122, 52)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.tool_running: fg=Some(Rgb(168, 91, 23)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.tool_success: fg=Some(Rgb(63, 107, 42)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.tool_failure: fg=Some(Rgb(174, 51, 39)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.selected: fg=Some(Rgb(70, 48, 31)) bg=Some(Rgb(234, 215, 180)) bold=true italic=false underline=false dim=false reverse=false
default.hover: fg=None bg=Some(Rgb(240, 223, 192)) bold=false italic=false underline=false dim=false reverse=false
default.decision.allow: fg=Some(Rgb(63, 107, 42)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.decision.deny.active: fg=Some(Rgb(174, 51, 39)) bg=Some(Rgb(243, 213, 201)) bold=true italic=false underline=false dim=false reverse=false
contrast.user: fg=Some(LightCyan) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.assistant: fg=Some(White) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.tool_success: fg=Some(LightGreen) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.error: fg=Some(LightRed) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.selected: fg=Some(Black) bg=Some(LightYellow) bold=true italic=false underline=false dim=false reverse=false
default.warning: fg=Some(Rgb(143, 100, 8)) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.warning: fg=Some(LightYellow) bg=None bold=true italic=false underline=false dim=false reverse=false
mono.warning: fg=None bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.heading: fg=Some(White) bg=None bold=true italic=false underline=true dim=false reverse=false
default.inline_code: fg=Some(Rgb(160, 58, 32)) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.inline_code: fg=Some(Black) bg=Some(LightYellow) bold=true italic=false underline=false dim=false reverse=false
mono.link: fg=None bg=None bold=true italic=false underline=true dim=false reverse=false
mono.inline_code: fg=None bg=None bold=true italic=false underline=false dim=false reverse=false
mono.selected: fg=None bg=None bold=false italic=false underline=false dim=false reverse=true
mono.hover: fg=None bg=None bold=false italic=false underline=true dim=false reverse=false
"#);
    }

    #[test]
    fn inline_code_uses_foreground_only_except_in_high_contrast() {
        for (theme, has_background) in [
            (Theme::new(ThemeKind::Default, ColorLevel::TrueColor), false),
            (Theme::new(ThemeKind::Default, ColorLevel::Ansi256), false),
            (Theme::new(ThemeKind::Default, ColorLevel::Ansi16), false),
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

    #[test]
    fn warm_background_bands_hand_pick_ansi256_cells() {
        // The computed RGB→cube quantization lands every warm beige on the
        // pink (255, 215, 215) cell 224. Each light band names its cube
        // index explicitly instead, and the ladder steps from light to
        // deep: cream surface, parchment code band, glaze hover, toasted
        // selection.
        let theme = Theme::new(ThemeKind::Default, ColorLevel::Ansi256);
        assert_eq!(theme.surface().bg, Some(Color::Indexed(231)));
        assert_eq!(theme.panel().bg, Some(Color::Indexed(231)));
        assert_eq!(theme.code_background(), Some(Color::Indexed(230)));
        assert_eq!(theme.hover().bg, Some(Color::Indexed(223)));
        assert_eq!(theme.hover_fill().bg, Some(Color::Indexed(223)));
        assert_eq!(theme.selected().bg, Some(Color::Indexed(222)));
        // Pane chrome keeps its yellow on ANSI-256 too: the quantizer would
        // gray BORDER out, so the border names its hand-picked cell.
        assert_eq!(theme.panel_border().fg, Some(Color::Indexed(137)));
        assert_eq!(
            Theme::new(ThemeKind::Default, ColorLevel::TrueColor)
                .panel_border()
                .fg,
            Some(Color::Rgb(0xAE, 0x8C, 0x5A))
        );
        // The default theme's hover on a capable terminal is the quiet
        // background alone — no underline, no foreground shift.
        assert!(!theme.hover().add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(theme.hover().fg, None);
        let truecolor = Theme::new(ThemeKind::Default, ColorLevel::TrueColor);
        assert!(
            !truecolor
                .hover()
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn text_selection_is_background_only_except_on_crude_targets() {
        // The documented contract: where subtle color exists the wash
        // touches only the background, so selected code keeps its syntax
        // colors.
        for theme in [
            Theme::new(ThemeKind::Default, ColorLevel::Ansi256),
            Theme::new(ThemeKind::Default, ColorLevel::TrueColor),
        ] {
            let style = theme.text_selection();
            assert_eq!(style.fg, None, "foreground preserved: {:?}", theme.key());
            assert!(style.bg.is_some(), "crust wash: {:?}", theme.key());
            assert!(!style.add_modifier.contains(Modifier::BOLD));
        }
        // The one documented exception: ANSI-16 and high-contrast text is
        // always bright, which a light-cyan wash would swallow, so the
        // pair is pinned black-on-light-cyan.
        for theme in [
            Theme::new(ThemeKind::Default, ColorLevel::Ansi16),
            Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16),
        ] {
            let style = theme.text_selection();
            assert_eq!(
                style.fg,
                Some(Color::Black),
                "pinned pair: {:?}",
                theme.key()
            );
            assert_eq!(
                style.bg,
                Some(Color::LightCyan),
                "pinned pair: {:?}",
                theme.key()
            );
        }
        // No color at all: bold reverse video, no color channels set.
        let mono = Theme::new(ThemeKind::Mono, ColorLevel::None).text_selection();
        assert_eq!(mono.fg, None);
        assert_eq!(mono.bg, None);
        assert!(
            mono.add_modifier
                .contains(Modifier::REVERSED | Modifier::BOLD)
        );
    }

    #[test]
    fn surfaces_and_interactions_degrade_gracefully_without_color() {
        let mono = Theme::new(ThemeKind::Mono, ColorLevel::None);
        assert_eq!(mono.surface(), Style::default());
        assert_eq!(mono.panel(), Style::default());
        assert_eq!(mono.code_background(), None);
        assert!(mono.selected().add_modifier.contains(Modifier::REVERSED));
        assert!(mono.hover().add_modifier.contains(Modifier::UNDERLINED));
        let active = mono.decision(DecisionTone::Allow, true);
        assert!(active.add_modifier.contains(Modifier::REVERSED));
        assert!(active.add_modifier.contains(Modifier::BOLD));

        let contrast = Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16);
        assert_eq!(contrast.surface(), Style::default());
        assert_eq!(contrast.code_background(), None);
        let ansi16 = Theme::new(ThemeKind::Default, ColorLevel::Ansi16);
        assert_eq!(ansi16.surface().fg, Some(Color::Black));
        assert_eq!(ansi16.surface().bg, Some(Color::White));
        // Hand-picked light-safe ANSI backgrounds, never nearest-color guesses.
        assert_eq!(ansi16.code_background(), Some(Color::Gray));
        assert_eq!(ansi16.selected().bg, Some(Color::Gray));
        assert_eq!(ansi16.hover().bg, Some(Color::Gray));
        assert!(ansi16.hover().add_modifier.contains(Modifier::UNDERLINED));

        let default = Theme::new(ThemeKind::Default, ColorLevel::TrueColor);
        for tone in [
            DecisionTone::Allow,
            DecisionTone::Deny,
            DecisionTone::Neutral,
        ] {
            let idle = default.decision(tone, false);
            let active = default.decision(tone, true);
            assert!(idle.bg.is_none(), "idle button has no fill: {tone:?}");
            assert!(active.bg.is_some(), "active button is filled: {tone:?}");
            assert_eq!(idle.fg, active.fg, "tone survives hover: {tone:?}");
        }
    }
}
