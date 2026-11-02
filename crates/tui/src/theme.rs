//! Semantic terminal themes and capability-aware color quantization.
//!
//! Quiet Ink (docs/site/specs/quiet-ink.md): neutral greys, one indigo
//! accent, and quiet tints in a light and a dark table that the web client
//! shares (crates/web/style/main.css). Every TrueColor foreground keeps
//! WCAG AA against its surface; ANSI-256 and ANSI-16 are hue-faithful
//! degradations. State is never conveyed by color alone (bold, italic,
//! underline, and text markers accompany every semantic color). Without
//! color (`NO_COLOR`, `TERM=dumb`) the same theme falls back to those
//! modifiers and reverse video.

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ThemeKind {
    Default,
    Dark,
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

/// One table's colours, by role.
#[derive(Debug)]
struct Palette {
    /// The frame's one surface.
    surface: Swatch,
    /// Fenced code and inline-code chips.
    sunken: Swatch,
    /// The band behind expanded tool output.
    terminal: Swatch,
    /// An expanded tool's title block: one step deeper than `terminal`.
    pan: Swatch,
    /// Hover fill.
    hover: Swatch,
    /// The keyboard-selected row.
    sel: Swatch,
    /// Mouse-drag text selection, deeper than `sel`.
    text_sel: Swatch,
    /// Pane and box borders.
    line: Swatch,
    /// Code-block borders and other stronger rules.
    line_strong: Swatch,
    fg: Swatch,
    /// Secondary text: tool rows, the agent's name.
    fg2: Swatch,
    muted: Swatch,
    /// Collapsed rows' summaries.
    quiet: Swatch,
    faint: Swatch,
    accent: Swatch,
    ok: Swatch,
    bad: Swatch,
    warn: Swatch,
    link: Swatch,
    code: Swatch,
    magenta: Swatch,
    orange: Swatch,
    teal: Swatch,
    allow_tint: Swatch,
    deny_tint: Swatch,
    neutral_tint: Swatch,
    added_tint: Swatch,
    removed_tint: Swatch,
}

impl Palette {
    fn swatches(&self) -> [Swatch; 28] {
        [
            self.surface,
            self.sunken,
            self.terminal,
            self.pan,
            self.hover,
            self.sel,
            self.text_sel,
            self.line,
            self.line_strong,
            self.fg,
            self.fg2,
            self.muted,
            self.quiet,
            self.faint,
            self.accent,
            self.ok,
            self.bad,
            self.warn,
            self.link,
            self.code,
            self.magenta,
            self.orange,
            self.teal,
            self.allow_tint,
            self.deny_tint,
            self.neutral_tint,
            self.added_tint,
            self.removed_tint,
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

const LIGHT: Palette = Palette {
    surface: swatch((0xFF, 0xFF, 0xFF), 231, Color::White),
    sunken: swatch((0xF4, 0xF4, 0xF5), 255, Color::White),
    terminal: swatch((0xF7, 0xF7, 0xF8), 255, Color::White),
    pan: swatch((0xEF, 0xEF, 0xF1), 255, Color::Gray),
    hover: swatch((0xE6, 0xE6, 0xEB), 254, Color::Gray),
    sel: swatch((0xEA, 0xEA, 0xFA), 189, Color::Gray),
    text_sel: swatch((0xD4, 0xD4, 0xF7), 189, Color::LightCyan),
    line: swatch((0xD4, 0xD4, 0xD8), 252, Color::Gray),
    line_strong: swatch((0xA1, 0xA1, 0xAA), 248, Color::DarkGray),
    fg: swatch((0x18, 0x18, 0x1B), 234, Color::Black),
    fg2: swatch((0x52, 0x52, 0x5B), 240, Color::DarkGray),
    muted: swatch((0x71, 0x71, 0x7A), 243, Color::DarkGray),
    quiet: swatch((0x52, 0x52, 0x5B), 240, Color::DarkGray),
    faint: swatch((0xA1, 0xA1, 0xAA), 248, Color::DarkGray),
    accent: swatch((0x5B, 0x5B, 0xD6), 62, Color::Blue),
    ok: swatch((0x26, 0x93, 0x5F), 29, Color::Green),
    bad: swatch((0xD1, 0x34, 0x3B), 160, Color::Red),
    warn: swatch((0xB2, 0x6B, 0x00), 136, Color::Yellow),
    link: swatch((0x1D, 0x6F, 0xB8), 25, Color::Blue),
    code: swatch((0x4A, 0x4A, 0xC2), 61, Color::Blue),
    magenta: swatch((0xA3, 0x39, 0x8E), 132, Color::Magenta),
    orange: swatch((0xC2, 0x41, 0x0C), 166, Color::Red),
    teal: swatch((0x0E, 0x7C, 0x86), 30, Color::Cyan),
    allow_tint: swatch((0xDF, 0xF3, 0xE8), 194, Color::LightGreen),
    deny_tint: swatch((0xFB, 0xE3, 0xE4), 224, Color::LightRed),
    neutral_tint: swatch((0xEC, 0xEC, 0xEF), 255, Color::Gray),
    added_tint: swatch((0xE3, 0xF4, 0xEA), 194, Color::LightGreen),
    removed_tint: swatch((0xFB, 0xE7, 0xE8), 224, Color::LightRed),
};

const DARK: Palette = Palette {
    surface: swatch((0x0E, 0x0E, 0x10), 233, Color::Black),
    sunken: swatch((0x1B, 0x1B, 0x1F), 234, Color::Black),
    terminal: swatch((0x15, 0x15, 0x18), 234, Color::Black),
    pan: swatch((0x1E, 0x1E, 0x23), 235, Color::DarkGray),
    hover: swatch((0x2A, 0x2A, 0x31), 237, Color::DarkGray),
    sel: swatch((0x23, 0x23, 0x45), 236, Color::DarkGray),
    text_sel: swatch((0x34, 0x34, 0x6B), 60, Color::Blue),
    line: swatch((0x2E, 0x2E, 0x34), 236, Color::DarkGray),
    line_strong: swatch((0x52, 0x52, 0x5B), 240, Color::DarkGray),
    fg: swatch((0xE4, 0xE4, 0xE7), 254, Color::White),
    fg2: swatch((0xB4, 0xB4, 0xBC), 250, Color::Gray),
    muted: swatch((0x8B, 0x8B, 0x94), 245, Color::Gray),
    quiet: swatch((0xA1, 0xA1, 0xAA), 248, Color::Gray),
    faint: swatch((0x5C, 0x5C, 0x66), 241, Color::DarkGray),
    accent: swatch((0x8F, 0x8F, 0xFF), 105, Color::LightBlue),
    ok: swatch((0x4C, 0xC3, 0x8A), 78, Color::LightGreen),
    bad: swatch((0xF2, 0x55, 0x5A), 203, Color::LightRed),
    warn: swatch((0xE5, 0xA9, 0x3B), 179, Color::LightYellow),
    link: swatch((0x79, 0xB8, 0xFF), 111, Color::LightBlue),
    code: swatch((0xA8, 0xA8, 0xFF), 147, Color::LightBlue),
    magenta: swatch((0xE0, 0x8A, 0xE0), 176, Color::LightMagenta),
    orange: swatch((0xFB, 0x92, 0x3C), 209, Color::LightRed),
    teal: swatch((0x5F, 0xD3, 0xDB), 80, Color::LightCyan),
    allow_tint: swatch((0x16, 0x35, 0x2A), 22, Color::Green),
    deny_tint: swatch((0x3A, 0x1A, 0x1C), 52, Color::Red),
    neutral_tint: swatch((0x27, 0x27, 0x2A), 235, Color::DarkGray),
    added_tint: swatch((0x12, 0x29, 0x1E), 22, Color::Green),
    removed_tint: swatch((0x2E, 0x15, 0x17), 52, Color::Red),
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
            "dark" | "dark-roast" | "darkroast" => ThemeKind::Dark,
            _ => ThemeKind::Default,
        };
        Self::from_kind_environment(requested_kind, no_color, term, colorterm)
    }

    /// The requested kind at the terminal's colour depth; `NO_COLOR` and
    /// `TERM=dumb` take the colour away, never the kind.
    pub fn from_kind_environment(
        kind: ThemeKind,
        no_color: bool,
        term: &str,
        colorterm: &str,
    ) -> Self {
        let colors = if no_color || term.eq_ignore_ascii_case("dumb") {
            ColorLevel::None
        } else if colorterm.eq_ignore_ascii_case("truecolor")
            || colorterm.eq_ignore_ascii_case("24bit")
        {
            ColorLevel::TrueColor
        } else if term.contains("256color") {
            ColorLevel::Ansi256
        } else {
            ColorLevel::Ansi16
        };
        Self::new(kind, colors)
    }

    pub const fn new(kind: ThemeKind, colors: ColorLevel) -> Self {
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
            ThemeKind::Default => &LIGHT,
        }
    }

    const fn has_color(&self) -> bool {
        !matches!(self.key.colors, ColorLevel::None)
    }

    /// Whether the terminal has the subtle steps (ANSI-256, true color) that
    /// tints and quiet fills need; sixteen colours keep flat blocks.
    const fn has_shades(&self) -> bool {
        matches!(self.key.colors, ColorLevel::Ansi256 | ColorLevel::TrueColor)
    }

    /// Base surface painted beneath the whole frame.
    pub fn surface(&self) -> Style {
        let palette = self.palette();
        match (
            self.swatch_color(palette.fg),
            self.swatch_color(palette.surface),
        ) {
            (Some(foreground), Some(background)) => Style::default().fg(foreground).bg(background),
            _ => Style::default(),
        }
    }

    /// Panels (overlays, pickers, the focused input box, the bottom bar)
    /// share the one surface; borders, not a second fill, delineate them.
    /// Foreground is left alone so content keeps whatever the surface
    /// painted.
    pub fn panel(&self) -> Style {
        self.swatch_color(self.palette().surface)
            .map_or_else(Style::default, |background| Style::default().bg(background))
    }

    /// Hairline border for pane chrome (pickers, panels, the composer).
    pub fn panel_border(&self) -> Style {
        self.semantic(self.palette().line, Modifier::DIM)
    }

    /// Keyboard-selected row in pickers, palettes, and the session tree: an
    /// indigo tint with bold text. Reverse video carries the selection where
    /// no color is available.
    pub fn selected(&self) -> Style {
        let style = Style::default().add_modifier(Modifier::BOLD);
        match self.key.colors {
            ColorLevel::None => Style::default().add_modifier(Modifier::REVERSED),
            ColorLevel::Ansi16 => style.fg(Color::Black).bg(self.palette().sel.ansi16),
            ColorLevel::Ansi256 | ColorLevel::TrueColor => {
                let palette = self.palette();
                let mut style = style;
                if let Some(foreground) = self.swatch_color(palette.fg) {
                    style = style.fg(foreground);
                }
                if let Some(background) = self.swatch_color(palette.sel) {
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

    /// Mouse-drag text selection: a deeper wash than the keyboard `selected`
    /// row and never bold, so the two selection kinds never read alike.
    /// Where subtle color exists only the background is set, so selected
    /// code keeps its syntax colors. Sixteen-colour text is always a bright
    /// color a wash would swallow, so the pair is pinned there; without
    /// colour, bold reverse video stays distinct from a selected row.
    pub fn text_selection(&self) -> Style {
        match self.key.colors {
            ColorLevel::None => Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            ColorLevel::Ansi16 => Style::default()
                .fg(Color::Black)
                .bg(self.palette().text_sel.ansi16),
            ColorLevel::Ansi256 | ColorLevel::TrueColor => self
                .swatch_color(self.palette().text_sel)
                .map_or_else(Style::default, |background| Style::default().bg(background)),
        }
    }

    /// Hover affordance patched over interactive text cells: a quiet fill
    /// one step off the surface. Sixteen colours and no colour underline as
    /// well, so the affordance never relies on a subtle background shift
    /// alone; without colour there is no background at all, so the cells
    /// reverse, and the underline keeps a cell that is already reversed (a
    /// selection) changing. None of it changes layout. Existing foreground
    /// colors are preserved by the patch.
    pub fn hover(&self) -> Style {
        let style = if self.has_shades() {
            Style::default()
        } else {
            Style::default().add_modifier(Modifier::UNDERLINED)
        };
        self.swatch_color(self.palette().hover).map_or_else(
            || style.add_modifier(Modifier::REVERSED),
            |background| style.bg(background),
        )
    }

    /// Hover fill for approval buttons and other glyph cells: exactly the
    /// text hover, underline included where the fill alone is too subtle.
    pub fn hover_fill(&self) -> Style {
        self.hover()
    }

    /// Band behind fenced code blocks.
    pub fn code_background(&self) -> Option<Color> {
        self.swatch_color(self.palette().sunken)
    }

    /// Band behind expanded tool output, a step quieter than code.
    pub fn terminal_background(&self) -> Option<Color> {
        self.swatch_color(self.palette().terminal)
    }

    /// Pale green row tint behind an added diff line. Sixteen-colour
    /// terminals have no pale step and keep the plain band.
    pub fn diff_added_background(&self) -> Option<Color> {
        self.shade(self.palette().added_tint)
    }

    /// Pale red row tint behind a removed diff line.
    pub fn diff_removed_background(&self) -> Option<Color> {
        self.shade(self.palette().removed_tint)
    }

    /// Background of an expanded tool's title block (header plus the rest of
    /// its primary argument): a grey one step deeper than the output band.
    pub fn tool_title_background(&self) -> Option<Color> {
        self.shade(self.palette().pan)
    }

    fn shade(&self, swatch: Swatch) -> Option<Color> {
        if self.has_shades() {
            self.swatch_color(swatch)
        } else {
            None
        }
    }

    /// Interactive block hover preserves foregrounds and never underlines.
    /// Without colour it is the fill's reverse video, in bold.
    pub fn block_hover(&self) -> Style {
        let style = self.hover_fill().remove_modifier(Modifier::UNDERLINED);
        if self.has_color() {
            style
        } else {
            style.add_modifier(Modifier::BOLD)
        }
    }

    /// One approval decision button. `active` (hover) fills the button with
    /// a decision-tinted background; the label colour and the button glyphs
    /// carry the meaning, never color alone.
    pub fn decision(&self, tone: DecisionTone, active: bool) -> Style {
        let palette = self.palette();
        let (foreground, tint) = match tone {
            DecisionTone::Allow => (palette.ok, palette.allow_tint),
            DecisionTone::Deny => (palette.bad, palette.deny_tint),
            DecisionTone::Neutral => (palette.fg2, palette.neutral_tint),
        };
        let style = Style::default().add_modifier(Modifier::BOLD);
        match self.key.colors {
            ColorLevel::None if active => style.add_modifier(Modifier::REVERSED),
            ColorLevel::None => style,
            ColorLevel::Ansi16 if active => style.fg(Color::Black).bg(tint.ansi16),
            ColorLevel::Ansi16 => style.fg(foreground.ansi16),
            ColorLevel::Ansi256 | ColorLevel::TrueColor => {
                let mut style = style;
                if let Some(foreground) = self.swatch_color(foreground) {
                    style = style.fg(foreground);
                }
                if active && let Some(background) = self.swatch_color(tint) {
                    style = style.bg(background);
                }
                style
            }
        }
    }

    pub fn body(&self) -> Style {
        Style::default()
    }

    pub fn muted(&self) -> Style {
        self.semantic(self.palette().muted, Modifier::DIM)
    }

    /// The faintest text: separators, line numbers, hints beside a hint.
    pub fn faint(&self) -> Style {
        self.semantic(self.palette().faint, Modifier::DIM)
    }

    /// The one accent, for the marks that say "this is yours" or "this is
    /// live": the user's prompt mark, the focused box, a selected bar.
    pub fn accent(&self) -> Style {
        self.semantic(self.palette().accent, Modifier::BOLD)
    }

    /// [`Self::muted`] faded `step` of `steps` of the way into the panel
    /// surface, for a status message dimming out. Without true colour there
    /// is no step between, so any fading step is dim.
    pub fn muted_fading(&self, step: u8, steps: u8) -> Style {
        let style = self.muted();
        if step == 0 {
            return style;
        }
        let surface = self.swatch_color(self.palette().surface);
        match (style.fg, surface) {
            (Some(Color::Rgb(r, g, b)), Some(Color::Rgb(sr, sg, sb))) => {
                let step = u16::from(step.min(steps));
                let steps = u16::from(steps.max(1));
                let blend = |from: u8, to: u8| {
                    let (from, to) = (u16::from(from), u16::from(to));
                    let mixed = (from * (steps - step) + to * step) / steps;
                    u8::try_from(mixed).unwrap_or(u8::MAX)
                };
                style.fg(Color::Rgb(blend(r, sr), blend(g, sg), blend(b, sb)))
            }
            _ => style.add_modifier(Modifier::DIM),
        }
    }

    /// The body text colour one step quieter, for rows that summarise hidden
    /// content: collapsed tool calls and thinking.
    pub fn muted_text(&self) -> Style {
        self.semantic(self.palette().quiet, Modifier::DIM)
    }

    pub fn user(&self) -> Style {
        self.semantic(self.palette().accent, Modifier::BOLD)
    }

    /// The Agents panel's keyboard cursor: the accent, or reverse video
    /// without colors, where bold alone would read as the watched session's
    /// bold.
    pub fn tree_cursor(&self) -> Style {
        if self.has_color() {
            self.accent()
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    pub fn assistant(&self) -> Style {
        self.semantic(self.palette().fg2, Modifier::BOLD)
    }

    pub fn thinking(&self) -> Style {
        self.semantic(self.palette().muted, Modifier::ITALIC)
    }

    /// Tool rows sit under the bold agent header, so colour alone carries
    /// them; colourless themes keep bold as the only cue left.
    pub fn tool(&self) -> Style {
        self.semantic(self.palette().fg2, self.weight_without_color())
    }

    pub fn tool_running(&self) -> Style {
        self.tool()
    }

    pub fn tool_success(&self) -> Style {
        self.semantic(self.palette().ok, self.weight_without_color())
    }

    /// Bold only where there is no colour to distinguish a row.
    fn weight_without_color(&self) -> Modifier {
        if self.has_color() {
            Modifier::empty()
        } else {
            Modifier::BOLD
        }
    }

    pub fn tool_failure(&self) -> Style {
        self.error()
    }

    /// Warning style: visually distinct from error styling (never red).
    pub fn warning(&self) -> Style {
        self.semantic(self.palette().warn, Modifier::BOLD)
    }

    pub fn error(&self) -> Style {
        self.semantic(self.palette().bad, Modifier::BOLD)
    }

    /// The colour of the tool group in slot `slot` of a tool list, cycling
    /// through eight hues, so one group's tools read alike.
    pub fn tool_group(&self, slot: usize) -> Style {
        let palette = self.palette();
        let hues = [
            palette.link,
            palette.ok,
            palette.magenta,
            palette.orange,
            palette.teal,
            palette.bad,
            palette.warn,
            palette.accent,
        ];
        self.semantic(hues[slot % hues.len()], Modifier::empty())
    }

    pub fn internal(&self) -> Style {
        self.semantic(self.palette().muted, Modifier::DIM)
    }

    pub fn heading(&self) -> Style {
        self.semantic(self.palette().fg, Modifier::BOLD)
    }

    pub fn link(&self) -> Style {
        self.semantic(self.palette().link, Modifier::UNDERLINED | Modifier::BOLD)
    }

    /// Inline code. With subtle colours it is text in the code colour with
    /// no tint, which is marker enough that the renderer drops the
    /// backticks and never breaks a line into padded blocks. Sixteen colours
    /// keep the body colour on the code band; without colour, bold text and
    /// the backticks.
    pub fn inline_code(&self) -> Style {
        if self.inline_code_is_coloured() {
            return self.semantic(self.palette().code, Modifier::empty());
        }
        if let Some(background) = self.code_background() {
            return self
                .semantic(self.palette().fg, Modifier::empty())
                .bg(background);
        }
        Style::default().add_modifier(Modifier::BOLD)
    }

    /// Whether inline code's own text colour marks it, so it needs neither a
    /// chip nor backticks.
    pub fn inline_code_is_coloured(&self) -> bool {
        self.has_shades()
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
        self.semantic(self.palette().line_strong, Modifier::DIM)
    }

    pub fn code_gutter(&self) -> Style {
        self.faint()
    }

    pub fn diff_added(&self) -> Style {
        self.semantic(self.palette().ok, Modifier::BOLD)
    }

    pub fn diff_removed(&self) -> Style {
        self.semantic(self.palette().bad, Modifier::BOLD)
    }

    pub fn diff_hunk(&self) -> Style {
        self.semantic(self.palette().link, Modifier::BOLD)
    }

    pub fn quote(&self) -> Style {
        self.semantic(self.palette().fg2, Modifier::ITALIC)
    }

    pub fn scrollbar_thumb(&self) -> Style {
        self.semantic(self.palette().line_strong, Modifier::DIM)
    }

    /// A scrollbar under the pointer, patched over its whole column: rail
    /// and thumb take the accent in bold. Without colour the rail sheds its
    /// dim and turns bold, so the bar still brightens where the already-bold
    /// thumb cannot.
    pub fn scrollbar_hover(&self) -> Style {
        self.semantic(self.palette().accent, Modifier::BOLD)
            .remove_modifier(Modifier::DIM)
    }

    pub fn input_border(&self, focused: bool) -> Style {
        if focused {
            self.semantic(self.palette().accent, Modifier::BOLD)
        } else {
            self.panel_border()
        }
    }

    pub fn quantize_rgb(&self, red: u8, green: u8, blue: u8) -> Option<Color> {
        match self.key.colors {
            ColorLevel::None => None,
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
            ColorLevel::Ansi16 => Some(swatch.ansi16),
            ColorLevel::Ansi256 => Some(Color::Indexed(swatch.ansi256)),
            ColorLevel::TrueColor => Some(Color::Rgb(swatch.rgb.0, swatch.rgb.1, swatch.rgb.2)),
        }
    }

    /// A foreground role. Colour carries the quiet roles, so `DIM` is only
    /// kept where there is no colour to do it.
    fn semantic(&self, swatch: Swatch, mut modifier: Modifier) -> Style {
        match self.swatch_color(swatch) {
            Some(color) => {
                modifier.remove(Modifier::DIM);
                Style::default().fg(color).add_modifier(modifier)
            }
            None => Style::default().add_modifier(modifier),
        }
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
