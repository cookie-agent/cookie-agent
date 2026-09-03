//! Semantic terminal themes and capability-aware color quantization.
//!
//! The default theme is a warm, light "cookie" palette: one cream surface
//! across the whole frame, espresso-brown text, and caramel/cinnamon/sage
//! accents. Every TrueColor foreground is chosen for WCAG-AA-or-better
//! contrast against the cream surface; ANSI-256 and ANSI-16 are hue-faithful
//! degradations. State is never conveyed by color alone (bold/italic/underline
//! and text markers accompany every semantic color). `Mono` drops all color,
//! `HighContrast` keeps bright ANSI colors on the terminal's own background.
//! The dark table re-lights every role; names denote hue, not value.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ThemeKind {
    Default,
    Dark,
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

#[derive(Clone, Copy, Debug)]
struct Swatch {
    rgb: (u8, u8, u8),
    ansi256: u8,
    ansi16: Color,
}

#[derive(Debug)]
struct Palette {
    cream: Swatch,
    parchment: Swatch,
    glaze: Swatch,
    toasted: Swatch,
    crust: Swatch,
    border: Swatch,
    tan: Swatch,
    espresso: Swatch,
    cocoa: Swatch,
    latte: Swatch,
    ash: Swatch,
    quote: Swatch,
    caramel: Swatch,
    sage: Swatch,
    basil: Swatch,
    cinnamon: Swatch,
    cranberry: Swatch,
    honey: Swatch,
    maple: Swatch,
    slate: Swatch,
    plum: Swatch,
    terracotta: Swatch,
    allow_tint: Swatch,
    deny_tint: Swatch,
    neutral_tint: Swatch,
}

impl Palette {
    fn swatches(&self) -> [Swatch; 25] {
        [
            self.cream,
            self.parchment,
            self.glaze,
            self.toasted,
            self.crust,
            self.border,
            self.tan,
            self.espresso,
            self.cocoa,
            self.latte,
            self.ash,
            self.quote,
            self.caramel,
            self.sage,
            self.basil,
            self.cinnamon,
            self.cranberry,
            self.honey,
            self.maple,
            self.slate,
            self.plum,
            self.terracotta,
            self.allow_tint,
            self.deny_tint,
            self.neutral_tint,
        ]
    }

    fn swatch_for_rgb(&self, rgb: (u8, u8, u8)) -> Option<Swatch> {
        self.swatches().into_iter().find(|swatch| swatch.rgb == rgb)
    }
}

const fn swatch(rgb: (u8, u8, u8), ansi256: u8, ansi16: Color) -> Swatch {
    Swatch {
        rgb,
        ansi256,
        ansi16,
    }
}

// Role names describe the bakery hue across both tables. The light table is
// the v2 palette; the dark table re-lights those roles for an espresso surface.
const LIGHT: Palette = Palette {
    cream: swatch((0xFB, 0xF4, 0xE6), 231, Color::White),
    parchment: swatch((0xF2, 0xE5, 0xCC), 230, Color::Gray),
    glaze: swatch((0xEB, 0xD8, 0xAE), 223, Color::Gray),
    toasted: swatch((0xE6, 0xCE, 0x9E), 222, Color::Gray),
    crust: swatch((0xC9, 0xAE, 0x85), 180, Color::LightCyan),
    border: swatch((0x6B, 0x4F, 0x2C), 240, Color::DarkGray),
    tan: swatch((0x71, 0x54, 0x30), 240, Color::DarkGray),
    espresso: swatch((0x46, 0x30, 0x1F), 236, Color::Black),
    cocoa: swatch((0x6E, 0x4E, 0x38), 239, Color::Cyan),
    latte: swatch((0x7A, 0x59, 0x41), 95, Color::DarkGray),
    ash: swatch((0x62, 0x5E, 0x66), 241, Color::DarkGray),
    quote: swatch((0x6F, 0x62, 0x50), 243, Color::DarkGray),
    caramel: swatch((0x9C, 0x5A, 0x10), 130, Color::Yellow),
    sage: swatch((0x4E, 0x7A, 0x34), 65, Color::Green),
    basil: swatch((0x2F, 0x6B, 0x38), 29, Color::Green),
    cinnamon: swatch((0x9C, 0x4A, 0x12), 131, Color::Yellow),
    cranberry: swatch((0xAE, 0x33, 0x27), 88, Color::Red),
    honey: swatch((0x7A, 0x52, 0x06), 94, Color::Yellow),
    maple: swatch((0x8A, 0x4A, 0x0B), 58, Color::Black),
    slate: swatch((0x3D, 0x6A, 0x8C), 24, Color::Blue),
    plum: swatch((0x8A, 0x55, 0x70), 96, Color::Magenta),
    terracotta: swatch((0xA8, 0x47, 0x1C), 124, Color::Red),
    allow_tint: swatch((0xDE, 0xE7, 0xC6), 194, Color::LightGreen),
    deny_tint: swatch((0xF3, 0xD5, 0xC9), 224, Color::LightRed),
    neutral_tint: swatch((0xE6, 0xDC, 0xC6), 187, Color::Gray),
};

const DARK: Palette = Palette {
    cream: swatch((0x20, 0x1C, 0x16), 234, Color::Black),
    parchment: swatch((0x39, 0x33, 0x26), 236, Color::DarkGray),
    glaze: swatch((0x4C, 0x43, 0x30), 238, Color::DarkGray),
    toasted: swatch((0x63, 0x56, 0x3B), 240, Color::Gray),
    crust: swatch((0x7E, 0x68, 0x47), 242, Color::LightCyan),
    border: swatch((0xC0, 0x99, 0x6A), 180, Color::DarkGray),
    tan: swatch((0xBB, 0x93, 0x61), 180, Color::DarkGray),
    espresso: swatch((0xED, 0xC7, 0xAB), 223, Color::White),
    cocoa: swatch((0xD4, 0x96, 0x6C), 173, Color::LightCyan),
    latte: swatch((0xBE, 0x8A, 0x65), 137, Color::Gray),
    ash: swatch((0x9B, 0x95, 0xA1), 246, Color::DarkGray),
    quote: swatch((0xA1, 0x8F, 0x74), 102, Color::DarkGray),
    caramel: swatch((0xC7, 0x77, 0x1E), 172, Color::Yellow),
    sage: swatch((0x60, 0x97, 0x40), 107, Color::LightGreen),
    basil: swatch((0x4A, 0xAA, 0x59), 71, Color::LightGreen),
    cinnamon: swatch((0xD9, 0x82, 0x46), 173, Color::Yellow),
    cranberry: swatch((0xF2, 0x6E, 0x61), 203, Color::LightRed),
    honey: swatch((0xCC, 0x95, 0x2D), 178, Color::Yellow),
    maple: swatch((0xE7, 0x8C, 0x33), 215, Color::White),
    slate: swatch((0x57, 0x97, 0xC8), 67, Color::LightBlue),
    plum: swatch((0xC5, 0x7A, 0xA0), 175, Color::LightMagenta),
    terracotta: swatch((0xE6, 0x6E, 0x3A), 209, Color::LightRed),
    allow_tint: swatch((0x82, 0x92, 0x57), 71, Color::LightGreen),
    deny_tint: swatch((0xBF, 0x7B, 0x5F), 203, Color::LightRed),
    neutral_tint: swatch((0x96, 0x8A, 0x71), 173, Color::Gray),
};

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
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let term = std::env::var("TERM").unwrap_or_default();
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        Self::from_kind_environment(kind, no_color, &term, &colorterm)
    }

    pub fn from_environment(requested: &str, no_color: bool, term: &str, colorterm: &str) -> Self {
        let requested_kind = match requested.to_ascii_lowercase().as_str() {
            "auto" => ThemeKind::Default,
            "dark" | "dark-roast" | "darkroast" => ThemeKind::Dark,
            "mono" | "monochrome" => ThemeKind::Mono,
            "high-contrast" | "high_contrast" | "contrast" => ThemeKind::HighContrast,
            _ => ThemeKind::Default,
        };
        Self::from_kind_environment(requested_kind, no_color, term, colorterm)
    }

    pub fn from_kind_environment(
        requested_kind: ThemeKind,
        no_color: bool,
        term: &str,
        colorterm: &str,
    ) -> Self {
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
            (ThemeKind::Default | ThemeKind::Dark, colors) => colors,
        };
        Self {
            key: ThemeKey { kind, colors },
        }
    }

    pub const fn key(&self) -> ThemeKey {
        self.key
    }

    const fn palette(&self) -> &'static Palette {
        match self.key.kind {
            ThemeKind::Dark => &DARK,
            ThemeKind::Default | ThemeKind::Mono | ThemeKind::HighContrast => &LIGHT,
        }
    }

    const fn is_bakery_palette(&self) -> bool {
        matches!(self.key.kind, ThemeKind::Default | ThemeKind::Dark)
    }

    /// Base surface painted beneath the bakery-palette UI. Mono and
    /// high-contrast themes keep the terminal background.
    pub fn surface(&self) -> Style {
        if !self.is_bakery_palette() {
            return Style::default();
        }
        let palette = self.palette();
        match self.key.colors {
            ColorLevel::None => Style::default(),
            ColorLevel::Ansi16 => Style::default()
                .fg(palette.espresso.ansi16)
                .bg(palette.cream.ansi16),
            ColorLevel::Ansi256 => Style::default()
                .fg(Color::Indexed(palette.espresso.ansi256))
                .bg(Color::Indexed(palette.cream.ansi256)),
            ColorLevel::TrueColor => Style::default()
                .fg(Color::Rgb(
                    palette.espresso.rgb.0,
                    palette.espresso.rgb.1,
                    palette.espresso.rgb.2,
                ))
                .bg(Color::Rgb(
                    palette.cream.rgb.0,
                    palette.cream.rgb.1,
                    palette.cream.rgb.2,
                )),
        }
    }

    /// Panels (overlays, pickers, the focused input box, the bottom bar)
    /// share the one cream surface — borders, not a second fill, delineate
    /// them. Foreground is left alone so content keeps whatever the surface
    /// painted.
    pub fn panel(&self) -> Style {
        self.background_color(self.palette().cream, None)
            .map_or_else(Style::default, |background| Style::default().bg(background))
    }

    /// Walnut border for pane chrome (conversation, tree, pickers), dark
    /// enough to keep the frame visible on the cream surface.
    pub fn panel_border(&self) -> Style {
        self.semantic(self.palette().border, Color::White, Modifier::DIM)
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
            ColorLevel::Ansi16 => style.fg(Color::Black).bg(self.palette().toasted.ansi16),
            ColorLevel::Ansi256 | ColorLevel::TrueColor => {
                let palette = self.palette();
                let mut style = style;
                if let Some(foreground) = self.swatch_color(palette.espresso) {
                    style = style.fg(foreground);
                }
                if let Some(background) = self.swatch_color(palette.toasted) {
                    style = style.bg(background);
                }
                style
            }
        }
    }

    /// Selection surface without foreground or emphasis, for rows whose spans
    /// retain their own visual hierarchy.
    pub fn selected_overlay(&self) -> Style {
        let selected = self.selected();
        let mut overlay = selected
            .bg
            .map_or_else(Style::default, |background| Style::default().bg(background));
        if selected.add_modifier.contains(Modifier::REVERSED) {
            overlay = overlay.add_modifier(Modifier::REVERSED);
        }
        overlay
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
            ColorLevel::Ansi16 => Style::default()
                .fg(Color::Black)
                .bg(self.palette().crust.ansi16),
            ColorLevel::Ansi256 | ColorLevel::TrueColor => self
                .swatch_color(self.palette().crust)
                .map_or_else(Style::default, |background| Style::default().bg(background)),
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
                ThemeKind::Default | ThemeKind::Dark,
                ColorLevel::Ansi256 | ColorLevel::TrueColor
            )
        );
        let style = if quiet_background_only {
            Style::default()
        } else {
            Style::default().add_modifier(Modifier::UNDERLINED)
        };
        self.background_color(self.palette().glaze, Some(Color::DarkGray))
            .map_or(style, |background| style.bg(background))
    }

    /// Background-only hover fill for approval buttons and other glyph
    /// cells where an underline would not read.
    pub fn hover_fill(&self) -> Style {
        self.background_color(self.palette().glaze, Some(Color::DarkGray))
            .map_or_else(Style::default, |background| Style::default().bg(background))
    }

    /// Subtle parchment band behind fenced code blocks. The bakery themes
    /// paint one; mono and high-contrast keep blocks flat.
    pub fn code_background(&self) -> Option<Color> {
        if !self.is_bakery_palette() {
            return None;
        }
        self.background_color(self.palette().parchment, None)
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
            let palette = self.palette();
            if active {
                let background = match tone {
                    DecisionTone::Allow => palette.allow_tint.ansi16,
                    DecisionTone::Deny => palette.deny_tint.ansi16,
                    DecisionTone::Neutral => palette.neutral_tint.ansi16,
                };
                return Style::default()
                    .fg(Color::Black)
                    .bg(background)
                    .add_modifier(Modifier::BOLD);
            }
            let foreground = match tone {
                DecisionTone::Allow => palette.basil.ansi16,
                DecisionTone::Deny => palette.cranberry.ansi16,
                DecisionTone::Neutral => palette.cocoa.ansi16,
            };
            return Style::default().fg(foreground).add_modifier(Modifier::BOLD);
        }
        let palette = self.palette();
        let (foreground, tint) = match tone {
            DecisionTone::Allow => (palette.basil, palette.allow_tint),
            DecisionTone::Deny => (palette.cranberry, palette.deny_tint),
            DecisionTone::Neutral => (palette.cocoa, palette.neutral_tint),
        };
        let mut style = Style::default().add_modifier(Modifier::BOLD);
        let foreground = if active && self.key.kind == ThemeKind::Dark {
            palette.cream
        } else {
            foreground
        };
        if let Some(foreground) = self.swatch_color(foreground) {
            style = style.fg(foreground);
        }
        if active && let Some(background) = self.swatch_color(tint) {
            style = style.bg(background);
        }
        style
    }

    pub fn body(&self) -> Style {
        Style::default()
    }

    pub fn muted(&self) -> Style {
        self.semantic(self.palette().latte, Color::White, Modifier::DIM)
    }

    pub fn user(&self) -> Style {
        self.semantic(self.palette().caramel, Color::LightCyan, Modifier::BOLD)
    }

    pub fn assistant(&self) -> Style {
        self.semantic(self.palette().sage, Color::White, Modifier::BOLD)
    }

    pub fn thinking(&self) -> Style {
        self.semantic(self.palette().plum, Color::LightMagenta, Modifier::ITALIC)
    }

    pub fn tool(&self) -> Style {
        self.semantic(self.palette().cinnamon, Color::LightYellow, Modifier::BOLD)
    }

    pub fn tool_running(&self) -> Style {
        self.tool()
    }

    pub fn tool_success(&self) -> Style {
        self.semantic(self.palette().basil, Color::LightGreen, Modifier::BOLD)
    }

    pub fn tool_failure(&self) -> Style {
        self.error()
    }

    /// Warning style — visually distinct from error styling (never red).
    pub fn warning(&self) -> Style {
        self.semantic(self.palette().honey, Color::LightYellow, Modifier::BOLD)
    }

    pub fn error(&self) -> Style {
        self.semantic(self.palette().cranberry, Color::LightRed, Modifier::BOLD)
    }

    pub fn internal(&self) -> Style {
        self.semantic(self.palette().ash, Color::Gray, Modifier::DIM)
    }

    pub fn heading(&self) -> Style {
        let style = self.semantic(self.palette().maple, Color::White, Modifier::BOLD);
        if self.key.kind == ThemeKind::HighContrast && self.key.colors != ColorLevel::None {
            style.add_modifier(Modifier::UNDERLINED)
        } else {
            style
        }
    }

    pub fn link(&self) -> Style {
        self.semantic(
            self.palette().slate,
            Color::LightCyan,
            Modifier::UNDERLINED | Modifier::BOLD,
        )
    }

    /// Inline code in assistant Markdown: a distinct warm terracotta
    /// foreground, never a background in the default theme — the source
    /// backticks stay visible, and the bold modifier carries the distinction
    /// in mono terminals. High contrast keeps its inverse-video chip.
    pub fn inline_code(&self) -> Style {
        let foreground = self.semantic(self.palette().terracotta, Color::Black, Modifier::BOLD);
        let background = match self.key.colors {
            ColorLevel::None => None,
            _ if self.key.kind == ThemeKind::HighContrast => Some(Color::LightYellow),
            ColorLevel::Ansi16 | ColorLevel::Ansi256 | ColorLevel::TrueColor => None,
        };
        background.map_or(foreground, |background| foreground.bg(background))
    }

    pub fn code_border(&self) -> Style {
        self.semantic(self.palette().tan, Color::White, Modifier::DIM)
    }

    pub fn quote(&self) -> Style {
        self.semantic(self.palette().quote, Color::LightMagenta, Modifier::ITALIC)
    }

    pub fn scrollbar_thumb(&self) -> Style {
        self.semantic(self.palette().espresso, Color::White, Modifier::DIM)
    }

    pub fn input_border(&self, focused: bool) -> Style {
        if focused {
            // The theme's highlight is honey yellow, not the reddish caramel
            // of user identity — focus reads as warmth, not as an error-adjacent red.
            if self.key.kind == ThemeKind::Dark && self.key.colors == ColorLevel::Ansi16 {
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                self.semantic(self.palette().honey, Color::LightYellow, Modifier::BOLD)
            }
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
                self.palette()
                    .swatch_for_rgb((red, green, blue))
                    .map_or_else(|| rgb_to_ansi256(red, green, blue), |swatch| swatch.ansi256),
            )),
            ColorLevel::Ansi16 => Some(
                self.palette()
                    .swatch_for_rgb((red, green, blue))
                    .map_or_else(|| nearest_ansi16(red, green, blue), |swatch| swatch.ansi16),
            ),
        }
    }

    fn swatch_color(&self, swatch: Swatch) -> Option<Color> {
        match self.key.colors {
            ColorLevel::None => None,
            _ if self.key.kind == ThemeKind::HighContrast => Some(nearest_high_contrast(
                swatch.rgb.0,
                swatch.rgb.1,
                swatch.rgb.2,
            )),
            ColorLevel::Ansi16 => Some(swatch.ansi16),
            ColorLevel::Ansi256 => Some(Color::Indexed(swatch.ansi256)),
            ColorLevel::TrueColor => Some(Color::Rgb(swatch.rgb.0, swatch.rgb.1, swatch.rgb.2)),
        }
    }

    /// A background color honoring kind/level fallbacks from its palette
    /// swatch. High contrast paints only explicitly requested backgrounds.
    fn background_color(&self, swatch: Swatch, high_contrast: Option<Color>) -> Option<Color> {
        match self.key.colors {
            ColorLevel::None => None,
            _ if self.key.kind == ThemeKind::HighContrast => high_contrast,
            ColorLevel::Ansi16 => Some(swatch.ansi16),
            ColorLevel::Ansi256 => Some(Color::Indexed(swatch.ansi256)),
            ColorLevel::TrueColor => Some(Color::Rgb(swatch.rgb.0, swatch.rgb.1, swatch.rgb.2)),
        }
    }

    fn semantic(&self, swatch: Swatch, high_contrast: Color, modifier: Modifier) -> Style {
        let mut modifier = match self.key.kind {
            ThemeKind::HighContrast => modifier | Modifier::BOLD,
            ThemeKind::Default | ThemeKind::Dark | ThemeKind::Mono => modifier,
        };
        if self.is_bakery_palette() {
            modifier.remove(Modifier::DIM);
        }
        let color = match self.key.colors {
            ColorLevel::None => None,
            _ if self.key.kind == ThemeKind::HighContrast => Some(high_contrast),
            ColorLevel::Ansi16 => Some(swatch.ansi16),
            ColorLevel::Ansi256 => Some(Color::Indexed(swatch.ansi256)),
            ColorLevel::TrueColor => Some(Color::Rgb(swatch.rgb.0, swatch.rgb.1, swatch.rgb.2)),
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
        for requested in ["dark", "dark-roast", "darkroast"] {
            assert_eq!(
                Theme::from_environment(requested, false, "xterm-256color", "").key(),
                super::ThemeKey {
                    kind: ThemeKind::Dark,
                    colors: ColorLevel::Ansi256,
                }
            );
        }
        for theme in [
            Theme::from_environment("mono", false, "xterm-256color", "truecolor"),
            Theme::from_environment("default", true, "xterm-256color", "truecolor"),
            Theme::from_environment("default", false, "dumb", "truecolor"),
            Theme::from_environment("dark", true, "xterm-256color", "truecolor"),
            Theme::from_environment("dark-roast", false, "dumb", "truecolor"),
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
    fn dark_semantic_theme_snapshot_is_deterministic() {
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

        let dark = Theme::new(ThemeKind::Dark, ColorLevel::TrueColor);
        let snapshot = [
            signature("dark.surface", dark.surface()),
            signature("dark.panel", dark.panel()),
            signature("dark.user", dark.user()),
            signature("dark.assistant", dark.assistant()),
            signature("dark.tool_running", dark.tool_running()),
            signature("dark.tool_success", dark.tool_success()),
            signature("dark.tool_failure", dark.tool_failure()),
            signature("dark.selected", dark.selected()),
            signature("dark.hover", dark.hover()),
            signature(
                "dark.decision.allow",
                dark.decision(DecisionTone::Allow, false),
            ),
            signature(
                "dark.decision.deny.active",
                dark.decision(DecisionTone::Deny, true),
            ),
            signature("dark.warning", dark.warning()),
            signature("dark.inline_code", dark.inline_code()),
        ]
        .join("\n");
        assert_snapshot!(snapshot, @r#"
dark.surface: fg=Some(Rgb(237, 199, 171)) bg=Some(Rgb(32, 28, 22)) bold=false italic=false underline=false dim=false reverse=false
dark.panel: fg=None bg=Some(Rgb(32, 28, 22)) bold=false italic=false underline=false dim=false reverse=false
dark.user: fg=Some(Rgb(199, 119, 30)) bg=None bold=true italic=false underline=false dim=false reverse=false
dark.assistant: fg=Some(Rgb(96, 151, 64)) bg=None bold=true italic=false underline=false dim=false reverse=false
dark.tool_running: fg=Some(Rgb(217, 130, 70)) bg=None bold=true italic=false underline=false dim=false reverse=false
dark.tool_success: fg=Some(Rgb(74, 170, 89)) bg=None bold=true italic=false underline=false dim=false reverse=false
dark.tool_failure: fg=Some(Rgb(242, 110, 97)) bg=None bold=true italic=false underline=false dim=false reverse=false
dark.selected: fg=Some(Rgb(237, 199, 171)) bg=Some(Rgb(99, 86, 59)) bold=true italic=false underline=false dim=false reverse=false
dark.hover: fg=None bg=Some(Rgb(76, 67, 48)) bold=false italic=false underline=false dim=false reverse=false
dark.decision.allow: fg=Some(Rgb(74, 170, 89)) bg=None bold=true italic=false underline=false dim=false reverse=false
dark.decision.deny.active: fg=Some(Rgb(32, 28, 22)) bg=Some(Rgb(191, 123, 95)) bold=true italic=false underline=false dim=false reverse=false
dark.warning: fg=Some(Rgb(204, 149, 45)) bg=None bold=true italic=false underline=false dim=false reverse=false
dark.inline_code: fg=Some(Rgb(230, 110, 58)) bg=None bold=true italic=false underline=false dim=false reverse=false
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
            (super::LIGHT.cream.rgb, 231),
            (super::LIGHT.parchment.rgb, 230),
            (super::LIGHT.glaze.rgb, 223),
            (super::LIGHT.toasted.rgb, 222),
            (super::LIGHT.crust.rgb, 180),
            (super::LIGHT.border.rgb, 240),
            (super::LIGHT.tan.rgb, 240),
            (super::LIGHT.honey.rgb, 94),
            (super::LIGHT.espresso.rgb, 236),
            (super::LIGHT.cocoa.rgb, 239),
            (super::LIGHT.latte.rgb, 95),
            (super::LIGHT.ash.rgb, 241),
            (super::LIGHT.quote.rgb, 243),
            (super::LIGHT.caramel.rgb, 130),
            (super::LIGHT.cinnamon.rgb, 131),
            (super::LIGHT.cranberry.rgb, 88),
            (super::LIGHT.maple.rgb, 58),
            (super::LIGHT.sage.rgb, 65),
            (super::LIGHT.basil.rgb, 29),
            (super::LIGHT.slate.rgb, 24),
            (super::LIGHT.plum.rgb, 96),
            (super::LIGHT.terracotta.rgb, 124),
            (super::LIGHT.allow_tint.rgb, 194),
            (super::LIGHT.deny_tint.rgb, 224),
            (super::LIGHT.neutral_tint.rgb, 187),
        ] {
            assert_eq!(
                theme.quantize_rgb(rgb.0, rgb.1, rgb.2),
                Some(Color::Indexed(cell)),
                "palette color {rgb:?}"
            );
        }
    }

    #[test]
    fn dark_palette_uses_hand_picked_ansi256_cells() {
        let theme = Theme::new(ThemeKind::Dark, ColorLevel::Ansi256);
        for swatch in super::DARK.swatches() {
            assert_eq!(
                theme.quantize_rgb(swatch.rgb.0, swatch.rgb.1, swatch.rgb.2),
                Some(Color::Indexed(swatch.ansi256)),
                "dark palette color {:?}",
                swatch.rgb
            );
        }
    }

    #[test]
    fn dark_palette_preserves_hues() {
        fn hsv_hue(rgb: (u8, u8, u8)) -> f64 {
            let red = f64::from(rgb.0) / 255.0;
            let green = f64::from(rgb.1) / 255.0;
            let blue = f64::from(rgb.2) / 255.0;
            let max = red.max(green).max(blue);
            let min = red.min(green).min(blue);
            let delta = max - min;
            if delta == 0.0 {
                0.0
            } else if max == red {
                60.0 * ((green - blue) / delta).rem_euclid(6.0)
            } else if max == green {
                60.0 * ((blue - red) / delta + 2.0)
            } else {
                60.0 * ((red - green) / delta + 4.0)
            }
        }

        let names = [
            "cream",
            "parchment",
            "glaze",
            "toasted",
            "crust",
            "border",
            "tan",
            "espresso",
            "cocoa",
            "latte",
            "ash",
            "quote",
            "caramel",
            "sage",
            "basil",
            "cinnamon",
            "cranberry",
            "honey",
            "maple",
            "slate",
            "plum",
            "terracotta",
            "allow_tint",
            "deny_tint",
            "neutral_tint",
        ];
        for ((name, light), dark) in names
            .into_iter()
            .zip(super::LIGHT.swatches())
            .zip(super::DARK.swatches())
        {
            let light_hue = hsv_hue(light.rgb);
            let dark_hue = hsv_hue(dark.rgb);
            let delta = (light_hue - dark_hue).abs();
            let delta = delta.min(360.0 - delta);
            // At LIGHT cream's 8% saturation, one 8-bit channel step moves
            // HSV hue by almost 3 degrees; allow that quantization interval.
            let tolerance = if name == "cream" { 4.0 } else { 2.0 };
            assert!(
                delta <= tolerance + 1e-9,
                "{name}: light={light_hue:.2} dark={dark_hue:.2} delta={delta:.2}"
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

        for colors in [
            ColorLevel::None,
            ColorLevel::Ansi16,
            ColorLevel::Ansi256,
            ColorLevel::TrueColor,
        ] {
            let dark = Theme::new(ThemeKind::Dark, colors);
            for (role, style) in [
                ("panel_border", dark.panel_border()),
                ("code_border", dark.code_border()),
                ("muted", dark.muted()),
                ("internal", dark.internal()),
                ("scrollbar_thumb", dark.scrollbar_thumb()),
            ] {
                assert!(
                    !style.add_modifier.contains(Modifier::DIM),
                    "Dark {role} retained DIM at {colors:?}"
                );
            }
        }
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
    fn dark_ansi16_uses_specified_roles() {
        let theme = Theme::new(ThemeKind::Dark, ColorLevel::Ansi16);
        assert_eq!(theme.surface().fg, Some(Color::White));
        assert_eq!(theme.surface().bg, Some(Color::Black));
        assert_eq!(theme.muted().fg, Some(Color::Gray));
        assert_eq!(theme.internal().fg, Some(Color::DarkGray));
        assert_eq!(theme.quote().fg, Some(Color::DarkGray));
        assert!(theme.quote().add_modifier.contains(Modifier::ITALIC));
        assert_eq!(theme.panel_border().fg, Some(Color::DarkGray));
        assert_eq!(theme.code_border().fg, Some(Color::DarkGray));
        assert_eq!(theme.input_border(true).fg, Some(Color::LightYellow));
        assert_eq!(theme.heading().fg, Some(Color::White));
        assert_eq!(theme.error().fg, Some(Color::LightRed));
        assert_eq!(theme.assistant().fg, Some(Color::LightGreen));
        assert_eq!(theme.tool_success().fg, Some(Color::LightGreen));
        assert_eq!(theme.thinking().fg, Some(Color::LightMagenta));
        assert_eq!(theme.link().fg, Some(Color::LightBlue));
        assert_eq!(theme.inline_code().fg, Some(Color::LightRed));
        assert_eq!(theme.user().fg, Some(Color::Yellow));
        assert_eq!(theme.tool().fg, Some(Color::Yellow));
        assert_eq!(theme.warning().fg, Some(Color::Yellow));
        assert_eq!(theme.selected().fg, Some(Color::Black));
        assert_eq!(theme.selected().bg, Some(Color::Gray));
        assert_eq!(theme.text_selection().fg, Some(Color::Black));
        assert_eq!(theme.text_selection().bg, Some(Color::LightCyan));
        assert_eq!(theme.hover().bg, Some(Color::DarkGray));
        assert!(theme.hover().add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(theme.code_background(), Some(Color::DarkGray));

        for (tone, idle_fg, active_bg) in [
            (DecisionTone::Allow, Color::LightGreen, Color::LightGreen),
            (DecisionTone::Deny, Color::LightRed, Color::LightRed),
            (DecisionTone::Neutral, Color::LightCyan, Color::Gray),
        ] {
            let idle = theme.decision(tone, false);
            let active = theme.decision(tone, true);
            assert_eq!(idle.fg, Some(idle_fg));
            assert_eq!(idle.bg, None);
            assert_eq!(active.fg, Some(Color::Black));
            assert_eq!(active.bg, Some(active_bg));
            assert!(idle.add_modifier.contains(Modifier::BOLD));
            assert!(active.add_modifier.contains(Modifier::BOLD));
        }
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

        for theme in [
            Theme::new(ThemeKind::Default, ColorLevel::TrueColor),
            Theme::new(ThemeKind::Dark, ColorLevel::TrueColor),
        ] {
            for tone in [
                DecisionTone::Allow,
                DecisionTone::Deny,
                DecisionTone::Neutral,
            ] {
                let idle = theme.decision(tone, false);
                let active = theme.decision(tone, true);
                assert!(idle.bg.is_none(), "idle button has no fill: {tone:?}");
                assert!(active.bg.is_some(), "active button is filled: {tone:?}");
                if theme.key().kind == ThemeKind::Dark {
                    assert_eq!(active.fg, theme.surface().bg, "surface on chip: {tone:?}");
                } else {
                    assert_eq!(idle.fg, active.fg, "tone survives hover: {tone:?}");
                }
            }
        }
    }
}
