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
    terminal: Swatch,
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
    /// Muted body text: inline code on the parchment tint.
    ink: Swatch,
}

impl Palette {
    fn swatches(&self) -> [Swatch; 27] {
        [
            self.cream,
            self.terminal,
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
            self.ink,
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
    terminal: swatch((0xF9, 0xF1, 0xE1), 230, Color::White),
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
    ink: swatch((0x62, 0x52, 0x40), 239, Color::DarkGray),
};

const DARK: Palette = Palette {
    cream: swatch((0x20, 0x1C, 0x16), 234, Color::Black),
    terminal: swatch((0x29, 0x25, 0x1D), 235, Color::Black),
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
    ink: swatch((0xC9, 0xB5, 0x9C), 180, Color::Gray),
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
        Self::from_environment(&requested, no_color, &term, &env_colorterm())
    }

    /// Apply a theme kind chosen by the TUI config file while still honoring
    /// `NO_COLOR`/`TERM=dumb` and detected terminal color capability.
    pub fn with_kind_from_env(kind: ThemeKind) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        let term = std::env::var("TERM").unwrap_or_default();
        Self::from_kind_environment(kind, no_color, &term, &env_colorterm())
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

    /// Barely tinted terminal band for shell commands and their output.
    pub fn terminal_background(&self) -> Option<Color> {
        if !self.is_bakery_palette() {
            return None;
        }
        self.background_color(self.palette().terminal, None)
    }

    /// Interactive block hover preserves foregrounds and never underlines.
    pub fn block_hover(&self) -> Style {
        let style = self.hover_fill().remove_modifier(Modifier::UNDERLINED);
        if self.key.colors == ColorLevel::None {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
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

    /// Tool rows sit under the bold agent header, so colour alone carries
    /// them; colourless themes keep bold as the only cue left.
    pub fn tool(&self) -> Style {
        self.semantic(
            self.palette().cinnamon,
            Color::LightYellow,
            self.weight_without_color(),
        )
    }

    pub fn tool_running(&self) -> Style {
        self.tool()
    }

    pub fn tool_success(&self) -> Style {
        self.semantic(
            self.palette().basil,
            Color::LightGreen,
            self.weight_without_color(),
        )
    }

    /// Bold only where there is no colour to distinguish a row.
    fn weight_without_color(&self) -> Modifier {
        if self.key.colors == ColorLevel::None {
            Modifier::BOLD
        } else {
            Modifier::empty()
        }
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
    /// Inline code. Bakery palettes set it in a muted body-text colour on the
    /// code-block parchment, in regular weight, which is marker enough that
    /// the renderer drops the backticks; other themes keep bold text and
    /// the backticks.
    pub fn inline_code(&self) -> Style {
        if let Some(background) = self.code_background() {
            // A muted take on the body text. Sixteen colours cannot mute it:
            // there the muted greys match the tint, so the chip keeps the
            // body text colour.
            let ink = if self.key.colors == ColorLevel::Ansi16 {
                self.palette().espresso
            } else {
                self.palette().ink
            };
            return self
                .semantic(ink, Color::Black, Modifier::empty())
                .bg(background);
        }
        let foreground = self.semantic(self.palette().terracotta, Color::Black, Modifier::BOLD);
        let background = match self.key.colors {
            ColorLevel::None => None,
            _ if self.key.kind == ThemeKind::HighContrast => Some(Color::LightYellow),
            ColorLevel::Ansi16 | ColorLevel::Ansi256 | ColorLevel::TrueColor => None,
        };
        background.map_or(foreground, |background| foreground.bg(background))
    }

    /// The half-block caps (`▐` before, `▌` after) that widen a tinted
    /// inline-code chip by half a cell on each side: foreground in the chip's
    /// background colour over the surrounding surface. `None` where inline
    /// code has no tint.
    pub fn inline_code_cap(&self) -> Option<Style> {
        self.inline_code()
            .bg
            .map(|background| Style::default().fg(background))
    }

    pub fn code_border(&self) -> Style {
        self.semantic(self.palette().tan, Color::White, Modifier::DIM)
    }

    pub fn code_gutter(&self) -> Style {
        self.muted()
    }

    pub fn diff_added(&self) -> Style {
        self.semantic(self.palette().basil, Color::LightGreen, Modifier::BOLD)
    }

    pub fn diff_removed(&self) -> Style {
        self.semantic(self.palette().cranberry, Color::LightRed, Modifier::BOLD)
    }

    pub fn diff_hunk(&self) -> Style {
        self.semantic(self.palette().slate, Color::LightCyan, Modifier::BOLD)
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

/// `COLORTERM`, or `truecolor` for a terminal known to render 24-bit colour
/// that does not advertise it there: Windows Terminal (`WT_SESSION`, which it
/// also shares into WSL) and a few `TERM_PROGRAM` identities.
fn env_colorterm() -> String {
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    colorterm_with_hints(
        colorterm,
        std::env::var_os("WT_SESSION").is_some(),
        &term_program,
    )
}

fn colorterm_with_hints(colorterm: String, windows_terminal: bool, term_program: &str) -> String {
    let known_truecolor =
        windows_terminal || matches!(term_program, "vscode" | "iTerm.app" | "WezTerm" | "ghostty");
    if colorterm.is_empty() && known_truecolor {
        "truecolor".to_owned()
    } else {
        colorterm
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
mod tests;
