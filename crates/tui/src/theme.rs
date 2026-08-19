//! Semantic terminal themes and capability-aware color quantization.
//!
//! The default theme is a warm, light "cookie" palette: one cream surface
//! across the whole frame, espresso-brown text, and caramel/cinnamon/sage
//! accents. Every TrueColor foreground is chosen for WCAG-AA-or-better
//! contrast against the cream surface; ANSI-256 and ANSI-16 are hue-faithful
//! degradations. State is never conveyed by color alone (bold/italic/underline
//! and text markers accompany every semantic color). `Mono` drops all color,
//! `HighContrast` keeps bright ANSI colors on the terminal's own background.

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
// glaze hover, walnut borders. Text: espresso body with cocoa, latte, and
// oven-ash derivatives. Accents: honey focus, caramel, sage, emerald basil,
// cinnamon, cranberry, maple, slate, plum, terracotta.

const CREAM: (u8, u8, u8) = (0xFB, 0xF4, 0xE6);
const PARCHMENT: (u8, u8, u8) = (0xF2, 0xE5, 0xCC);
const TOASTED: (u8, u8, u8) = (0xE6, 0xCE, 0x9E);
const GLAZE: (u8, u8, u8) = (0xEB, 0xD8, 0xAE);
const CRUST: (u8, u8, u8) = (0xC9, 0xAE, 0x85);
// Pane chrome (conversation, tree, pickers, unfocused input) uses a walnut
// border that reads clearly against the cream surface.
const BORDER: (u8, u8, u8) = (0x6B, 0x4F, 0x2C);
const ESPRESSO: (u8, u8, u8) = (0x46, 0x30, 0x1F);
const COCOA: (u8, u8, u8) = (0x6E, 0x4E, 0x38);
const LATTE: (u8, u8, u8) = (0x7A, 0x59, 0x41);
const ASH: (u8, u8, u8) = (0x62, 0x5E, 0x66);
const CARAMEL: (u8, u8, u8) = (0x9C, 0x5A, 0x10);
const SAGE: (u8, u8, u8) = (0x4E, 0x7A, 0x34);
const BASIL: (u8, u8, u8) = (0x2F, 0x6B, 0x38);
const CINNAMON: (u8, u8, u8) = (0x9C, 0x4A, 0x12);
const CRANBERRY: (u8, u8, u8) = (0xAE, 0x33, 0x27);
const HONEY: (u8, u8, u8) = (0x7A, 0x52, 0x06);
const MAPLE: (u8, u8, u8) = (0x8A, 0x4A, 0x0B);
const SLATE: (u8, u8, u8) = (0x3D, 0x6A, 0x8C);
const PLUM: (u8, u8, u8) = (0x8A, 0x55, 0x70);
const TAN: (u8, u8, u8) = (0x71, 0x54, 0x30);
const TERRACOTTA: (u8, u8, u8) = (0xA8, 0x47, 0x1C);
const QUOTE: (u8, u8, u8) = (0x6F, 0x62, 0x50);
const ALLOW_TINT: (u8, u8, u8) = (0xDE, 0xE7, 0xC6);
const DENY_TINT: (u8, u8, u8) = (0xF3, 0xD5, 0xC9);
const NEUTRAL_TINT: (u8, u8, u8) = (0xE6, 0xDC, 0xC6);

// --- Hand-picked xterm-256 palette cells -----------------------------------
//
// These cells preserve the palette's visual hierarchy and hue more faithfully
// than naive RGB-to-cube rounding. Non-palette colors still use that fallback.
const CREAM_256: u8 = 231; // (255, 255, 255)
const PARCHMENT_256: u8 = 230; // (255, 255, 215)
const GLAZE_256: u8 = 223; // (255, 215, 175)
const TOASTED_256: u8 = 222; // (255, 215, 135)
const CRUST_256: u8 = 180; // (215, 175, 135)
const BORDER_256: u8 = 240; // (88, 88, 88)
const ESPRESSO_256: u8 = 236; // (48, 48, 48)
const COCOA_256: u8 = 239; // (78, 78, 78)
const LATTE_256: u8 = 95; // (135, 95, 95)
const ASH_256: u8 = 241; // (98, 98, 98)
const CARAMEL_256: u8 = 130; // (175, 95, 0)
const SAGE_256: u8 = 65; // (95, 135, 95)
const BASIL_256: u8 = 29; // (0, 135, 95)
const CINNAMON_256: u8 = 131; // (175, 95, 95)
const CRANBERRY_256: u8 = 88; // (135, 0, 0)
const HONEY_256: u8 = 94; // (135, 95, 0)
const MAPLE_256: u8 = 58; // (95, 95, 0)
const SLATE_256: u8 = 24; // (0, 95, 135)
const PLUM_256: u8 = 96; // (135, 95, 135)
const TAN_256: u8 = 240; // (88, 88, 88)
const TERRACOTTA_256: u8 = 124; // (175, 0, 0)
const QUOTE_256: u8 = 243; // (118, 118, 118)
const ALLOW_TINT_256: u8 = 194; // (215, 255, 215)
const DENY_TINT_256: u8 = 224; // (255, 215, 215)
const NEUTRAL_TINT_256: u8 = 187; // (215, 215, 175)

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

    /// Walnut border for pane chrome (conversation, tree, pickers), dark
    /// enough to keep the frame visible on the cream surface.
    pub fn panel_border(&self) -> Style {
        self.semantic(BORDER, Color::DarkGray, Color::White, Modifier::DIM)
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
            DecisionTone::Allow => (BASIL, ALLOW_TINT),
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
        self.semantic(BASIL, Color::Green, Color::LightGreen, Modifier::BOLD)
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
        self.semantic(ASH, Color::DarkGray, Color::Gray, Modifier::DIM)
    }

    pub fn heading(&self) -> Style {
        let style = self.semantic(MAPLE, Color::Black, Color::White, Modifier::BOLD);
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
        self.semantic(TAN, Color::DarkGray, Color::White, Modifier::DIM)
    }

    pub fn quote(&self) -> Style {
        self.semantic(
            QUOTE,
            Color::DarkGray,
            Color::LightMagenta,
            Modifier::ITALIC,
        )
    }

    pub fn scrollbar_thumb(&self) -> Style {
        self.semantic(ESPRESSO, Color::Black, Color::White, Modifier::DIM)
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
            ColorLevel::Ansi256 => Some(Color::Indexed(
                palette_ansi256((red, green, blue))
                    .unwrap_or_else(|| rgb_to_ansi256(red, green, blue)),
            )),
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
        let mut modifier = match self.key.kind {
            ThemeKind::HighContrast => modifier | Modifier::BOLD,
            ThemeKind::Default | ThemeKind::Mono => modifier,
        };
        if self.key.kind == ThemeKind::Default && self.key.colors != ColorLevel::None {
            modifier.remove(Modifier::DIM);
        }
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

fn palette_ansi256(rgb: (u8, u8, u8)) -> Option<u8> {
    match rgb {
        CREAM => Some(CREAM_256),
        PARCHMENT => Some(PARCHMENT_256),
        GLAZE => Some(GLAZE_256),
        TOASTED => Some(TOASTED_256),
        CRUST => Some(CRUST_256),
        BORDER => Some(BORDER_256),
        ESPRESSO => Some(ESPRESSO_256),
        COCOA => Some(COCOA_256),
        LATTE => Some(LATTE_256),
        ASH => Some(ASH_256),
        CARAMEL => Some(CARAMEL_256),
        SAGE => Some(SAGE_256),
        BASIL => Some(BASIL_256),
        CINNAMON => Some(CINNAMON_256),
        CRANBERRY => Some(CRANBERRY_256),
        HONEY => Some(HONEY_256),
        MAPLE => Some(MAPLE_256),
        SLATE => Some(SLATE_256),
        PLUM => Some(PLUM_256),
        TAN => Some(TAN_256),
        TERRACOTTA => Some(TERRACOTTA_256),
        QUOTE => Some(QUOTE_256),
        ALLOW_TINT => Some(ALLOW_TINT_256),
        DENY_TINT => Some(DENY_TINT_256),
        NEUTRAL_TINT => Some(NEUTRAL_TINT_256),
        _ => None,
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
default.tool_running: fg=Some(Rgb(156, 74, 18)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.tool_success: fg=Some(Rgb(47, 107, 56)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.tool_failure: fg=Some(Rgb(174, 51, 39)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.selected: fg=Some(Rgb(70, 48, 31)) bg=Some(Rgb(230, 206, 158)) bold=true italic=false underline=false dim=false reverse=false
default.hover: fg=None bg=Some(Rgb(235, 216, 174)) bold=false italic=false underline=false dim=false reverse=false
default.decision.allow: fg=Some(Rgb(47, 107, 56)) bg=None bold=true italic=false underline=false dim=false reverse=false
default.decision.deny.active: fg=Some(Rgb(174, 51, 39)) bg=Some(Rgb(243, 213, 201)) bold=true italic=false underline=false dim=false reverse=false
contrast.user: fg=Some(LightCyan) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.assistant: fg=Some(White) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.tool_success: fg=Some(LightGreen) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.error: fg=Some(LightRed) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.selected: fg=Some(Black) bg=Some(LightYellow) bold=true italic=false underline=false dim=false reverse=false
default.warning: fg=Some(Rgb(122, 82, 6)) bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.warning: fg=Some(LightYellow) bg=None bold=true italic=false underline=false dim=false reverse=false
mono.warning: fg=None bg=None bold=true italic=false underline=false dim=false reverse=false
contrast.heading: fg=Some(White) bg=None bold=true italic=false underline=true dim=false reverse=false
default.inline_code: fg=Some(Rgb(168, 71, 28)) bg=None bold=true italic=false underline=false dim=false reverse=false
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
        // Each light band names its cell explicitly, and the ladder steps
        // from light to deep: cream surface, parchment code band, glaze
        // hover, toasted selection.
        let theme = Theme::new(ThemeKind::Default, ColorLevel::Ansi256);
        assert_eq!(theme.surface().bg, Some(Color::Indexed(231)));
        assert_eq!(theme.panel().bg, Some(Color::Indexed(231)));
        assert_eq!(theme.code_background(), Some(Color::Indexed(230)));
        assert_eq!(theme.hover().bg, Some(Color::Indexed(223)));
        assert_eq!(theme.hover_fill().bg, Some(Color::Indexed(223)));
        assert_eq!(theme.selected().bg, Some(Color::Indexed(222)));
        assert_eq!(
            theme.quantize_rgb(0xEB, 0xD8, 0xAE),
            Some(Color::Indexed(223))
        );
        assert_eq!(
            theme.quantize_rgb(0xE6, 0xCE, 0x9E),
            Some(Color::Indexed(222))
        );
        // Pane chrome uses the hand-picked walnut cell.
        assert_eq!(theme.panel_border().fg, Some(Color::Indexed(240)));
        assert_eq!(
            Theme::new(ThemeKind::Default, ColorLevel::TrueColor)
                .panel_border()
                .fg,
            Some(Color::Rgb(0x6B, 0x4F, 0x2C))
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
    fn palette_colors_use_hand_picked_ansi256_cells() {
        let theme = Theme::new(ThemeKind::Default, ColorLevel::Ansi256);
        for (rgb, cell) in [
            (super::CREAM, 231),
            (super::PARCHMENT, 230),
            (super::GLAZE, 223),
            (super::TOASTED, 222),
            (super::CRUST, 180),
            (super::BORDER, 240),
            (super::TAN, 240),
            (super::HONEY, 94),
            (super::ESPRESSO, 236),
            (super::COCOA, 239),
            (super::LATTE, 95),
            (super::ASH, 241),
            (super::QUOTE, 243),
            (super::CARAMEL, 130),
            (super::CINNAMON, 131),
            (super::CRANBERRY, 88),
            (super::MAPLE, 58),
            (super::SAGE, 65),
            (super::BASIL, 29),
            (super::SLATE, 24),
            (super::PLUM, 96),
            (super::TERRACOTTA, 124),
            (super::ALLOW_TINT, 194),
            (super::DENY_TINT, 224),
            (super::NEUTRAL_TINT, 187),
        ] {
            assert_eq!(
                theme.quantize_rgb(rgb.0, rgb.1, rgb.2),
                Some(Color::Indexed(cell)),
                "palette color {rgb:?}"
            );
        }
    }

    #[test]
    fn default_quiet_roles_use_values_without_dim() {
        let truecolor = Theme::new(ThemeKind::Default, ColorLevel::TrueColor);
        for style in [
            truecolor.panel_border(),
            truecolor.code_border(),
            truecolor.muted(),
            truecolor.internal(),
        ] {
            assert!(!style.add_modifier.contains(Modifier::DIM));
        }
        assert_eq!(
            truecolor.input_border(true).fg,
            Some(Color::Rgb(0x7A, 0x52, 0x06))
        );
        assert!(
            truecolor
                .input_border(true)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !truecolor
                .input_border(true)
                .add_modifier
                .contains(Modifier::DIM)
        );

        let mono = Theme::new(ThemeKind::Mono, ColorLevel::None);
        assert!(mono.panel_border().add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn default_ansi16_uses_revised_quiet_roles() {
        let theme = Theme::new(ThemeKind::Default, ColorLevel::Ansi16);
        for style in [
            theme.panel_border(),
            theme.code_border(),
            theme.internal(),
            theme.muted(),
        ] {
            assert_eq!(style.fg, Some(Color::DarkGray));
            assert!(!style.add_modifier.contains(Modifier::DIM));
        }
        assert_eq!(theme.quote().fg, Some(Color::DarkGray));
        assert!(theme.quote().add_modifier.contains(Modifier::ITALIC));
        assert_eq!(theme.heading().fg, Some(Color::Black));
        assert!(theme.heading().add_modifier.contains(Modifier::BOLD));
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
