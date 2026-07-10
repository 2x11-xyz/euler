use super::syntax::SyntaxKind;
use ratatui::style::{Color, Modifier, Style};

pub(crate) use crate::theme_catalog::ThemeChoice;

const GRUVBOX_DARK_BACKGROUND: Color = Color::Rgb(40, 40, 40);
const GRUVBOX_DARK_FOREGROUND: Color = Color::Rgb(235, 219, 178);
const GRUVBOX_LIGHT_BACKGROUND: Color = Color::Rgb(251, 241, 199);
const GRUVBOX_LIGHT_FOREGROUND: Color = Color::Rgb(60, 56, 54);
pub(crate) const USER_RAIL_COLOR: Color = Color::Rgb(142, 192, 124);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorLevel {
    TrueColor,
    Indexed256,
    Basic16,
}

impl ColorLevel {
    pub const SUPPORTED: [Self; 3] = [Self::TrueColor, Self::Indexed256, Self::Basic16];

    /// Detect truecolor support once at startup (#64) from the real process
    /// environment. See [`Self::from_env_signals`] for the decision rule.
    pub fn detect() -> Self {
        Self::from_env_signals(
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var("TERM_PROGRAM").ok().as_deref(),
        )
    }

    /// Pure decision rule behind [`Self::detect`], taking the two signals as
    /// plain strings so it's testable without touching process env vars.
    ///
    /// `COLORTERM=truecolor`/`=24bit` is a positive signal and wins outright.
    /// Otherwise a conservative denylist of terminals known to mangle 24-bit
    /// SGR sequences into two or three colors — `TERM_PROGRAM=Apple_Terminal`
    /// (#64) — forces the ANSI-256 fallback. Everything else defaults to
    /// truecolor, matching prior behavior for terminals that don't advertise
    /// either signal.
    pub(crate) fn from_env_signals(colorterm: Option<&str>, term_program: Option<&str>) -> Self {
        let advertises_truecolor = colorterm
            .map(|value| {
                value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
            })
            .unwrap_or(false);
        if advertises_truecolor {
            return Self::TrueColor;
        }
        if term_program == Some("Apple_Terminal") {
            return Self::Indexed256;
        }
        Self::TrueColor
    }

    pub fn quantize(self, color: Color) -> Color {
        match (self, color) {
            (_, Color::Reset) | (ColorLevel::TrueColor, _) => color,
            (ColorLevel::Indexed256, Color::Rgb(red, green, blue)) => {
                Color::Indexed(nearest_xterm_index(red, green, blue))
            }
            (ColorLevel::Indexed256, _) => color,
            (ColorLevel::Basic16, Color::Rgb(red, green, blue)) => {
                nearest_basic_color(red, green, blue)
            }
            (ColorLevel::Basic16, Color::Indexed(index)) => {
                let (red, green, blue) = xterm_rgb(index);
                nearest_basic_color(red, green, blue)
            }
            (ColorLevel::Basic16, _) => color,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackgroundMode {
    Transparent,
    Opaque(Color),
}

impl BackgroundMode {
    pub const DEFAULT_DARK_OPAQUE: Self = Self::Opaque(GRUVBOX_DARK_BACKGROUND);
    // Opaque is the default so terminal cells are painted with the theme color
    // instead of inheriting a dark emulator background behind a light surface.
    pub const DEFAULT_DARK_BACKGROUNDS: [Self; 2] =
        [Self::DEFAULT_DARK_OPAQUE, Self::DEFAULT_DARK_TRANSPARENT];
    pub const DEFAULT_DARK_TRANSPARENT: Self = Self::Transparent;

    fn resolved(self, fallback: Color) -> Color {
        match self {
            Self::Transparent => Color::Reset,
            Self::Opaque(Color::Reset) => fallback,
            Self::Opaque(color) => color,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThemeOptions {
    pub color_level: ColorLevel,
    pub background: BackgroundMode,
}

impl ThemeOptions {
    pub fn default_dark() -> Self {
        let [color_level, _, _] = ColorLevel::SUPPORTED;
        let [background, _] = BackgroundMode::DEFAULT_DARK_BACKGROUNDS;
        Self {
            color_level,
            background,
        }
    }

    pub fn default_light() -> Self {
        let [color_level, _, _] = ColorLevel::SUPPORTED;
        Self {
            color_level,
            background: BackgroundMode::Opaque(GRUVBOX_LIGHT_BACKGROUND),
        }
    }
}

impl Default for ThemeOptions {
    fn default() -> Self {
        Self::default_dark()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Theme {
    pub palette: Palette,
    pub activity: ActivityTheme,
    pub banner: BannerTheme,
    pub composer: ComposerTheme,
    pub status: StatusTheme,
    pub transcript: TranscriptTheme,
    pub scopes: SemanticTheme,
    pub surfaces: SurfaceThemes,
    pub color_level: ColorLevel,
    pub background: BackgroundMode,
}

impl Theme {
    /// Production entry point (#64): the color level should always come
    /// from [`ColorLevel::detect`] (or a preserved prior value on a theme
    /// switch) rather than defaulting to truecolor, so every theme RGB
    /// quantizes at the theme boundary when the terminal can't render
    /// 24-bit color.
    pub fn for_choice_with_color_level(choice: ThemeChoice, color_level: ColorLevel) -> Self {
        match choice {
            ThemeChoice::GruvboxDark => Self::default_dark_with(ThemeOptions {
                color_level,
                ..ThemeOptions::default_dark()
            }),
            ThemeChoice::GruvboxLight => Self::default_light_with(ThemeOptions {
                color_level,
                ..ThemeOptions::default_light()
            }),
            ThemeChoice::WarmLedger => Self::warm_ledger_with(ThemeOptions {
                color_level,
                background: BackgroundMode::Opaque(Color::Rgb(0x26, 0x23, 0x19)),
            }),
        }
    }

    /// Truecolor convenience constructor kept for tests that don't care
    /// about color-level fallback; production always goes through
    /// [`Self::for_choice_with_color_level`].
    #[cfg(test)]
    pub fn warm_ledger() -> Self {
        Self::warm_ledger_with(ThemeOptions {
            color_level: ColorLevel::TrueColor,
            background: BackgroundMode::Opaque(Color::Rgb(0x26, 0x23, 0x19)),
        })
    }

    pub fn warm_ledger_with(options: ThemeOptions) -> Self {
        let palette = PaletteSeed::warm_ledger().resolve(options);
        Self::from_palette(palette, options)
    }

    pub fn default_dark() -> Self {
        Self::default_dark_with(ThemeOptions::default_dark())
    }

    /// Truecolor convenience constructor kept for tests; see
    /// [`Self::warm_ledger`].
    #[cfg(test)]
    pub fn default_light() -> Self {
        Self::default_light_with(ThemeOptions::default_light())
    }

    pub fn default_dark_with(options: ThemeOptions) -> Self {
        let palette = Palette::default_dark_with(options);
        Self::from_palette(palette, options)
    }

    pub fn default_light_with(options: ThemeOptions) -> Self {
        let palette = Palette::default_light_with(options);
        Self::from_palette(palette, options)
    }

    fn from_palette(palette: Palette, options: ThemeOptions) -> Self {
        Self {
            activity: ActivityTheme::from_palette(&palette),
            banner: BannerTheme::from_palette(&palette),
            composer: ComposerTheme::from_palette(&palette),
            status: StatusTheme::from_palette(&palette),
            transcript: TranscriptTheme::from_palette(&palette),
            scopes: SemanticTheme::from_palette(&palette),
            surfaces: SurfaceThemes::from_palette(&palette),
            palette,
            color_level: options.color_level,
            background: options.background,
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::default_dark()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Palette {
    pub foreground: Color,
    pub background: Color,
    pub surface: Color,
    pub surface_high: Color,
    pub selection: Color,
    pub hairline: Color,
    pub composer_rule: Color,
    pub user_rail: Color,
    pub queued_rail: Color,
    pub added: Color,
    pub removed: Color,
    pub changed: Color,
    pub added_tint: Color,
    pub removed_tint: Color,
    pub changed_tint: Color,
    pub muted: Color,
    pub warning: Color,
    pub error: Color,
    pub code: Color,
    pub user: Color,
    pub assistant: Color,
    pub tool: Color,
    pub gutter: Color,
    pub cursor: Color,
    pub st_state: Color,
    pub st_model: Color,
    pub st_cost: Color,
    pub st_ctx: Color,
}

impl Palette {
    pub fn default_dark_with(options: ThemeOptions) -> Self {
        PaletteSeed::default_dark().resolve(options)
    }

    pub fn default_light_with(options: ThemeOptions) -> Self {
        PaletteSeed::default_light().resolve(options)
    }
}

#[derive(Clone, Copy)]
struct PaletteSeed {
    foreground: Color,
    background: Color,
    surface: Color,
    surface_high: Color,
    selection: Color,
    hairline: Color,
    composer_rule: Color,
    user_rail: Color,
    queued_rail: Color,
    added: Color,
    removed: Color,
    changed: Color,
    added_tint_pct: u8,
    removed_tint_pct: u8,
    changed_tint_pct: u8,
    muted: Color,
    warning: Color,
    error: Color,
    code: Color,
    user: Color,
    assistant: Color,
    tool: Color,
    gutter: Color,
    cursor: Color,
    st_state: Color,
    st_model: Color,
    st_cost: Color,
    st_ctx: Color,
}

impl PaletteSeed {
    fn default_dark() -> Self {
        Self {
            foreground: GRUVBOX_DARK_FOREGROUND,
            background: GRUVBOX_DARK_BACKGROUND,
            surface: Color::Rgb(60, 56, 54),
            surface_high: Color::Rgb(80, 73, 69),
            selection: Color::Rgb(102, 92, 84),
            hairline: Color::Rgb(80, 73, 69),
            composer_rule: Color::Rgb(102, 92, 84),
            user_rail: USER_RAIL_COLOR,
            queued_rail: Color::Rgb(102, 92, 84),
            added: Color::Rgb(184, 187, 38),
            removed: Color::Rgb(251, 73, 52),
            changed: Color::Rgb(250, 189, 47),
            added_tint_pct: 28,
            removed_tint_pct: 28,
            changed_tint_pct: 24,
            muted: Color::Rgb(168, 153, 132),
            warning: Color::Rgb(254, 128, 25),
            error: Color::Rgb(251, 73, 52),
            code: Color::Rgb(250, 189, 47),
            user: USER_RAIL_COLOR,
            assistant: GRUVBOX_DARK_FOREGROUND,
            tool: Color::Rgb(131, 165, 152),
            gutter: Color::Rgb(124, 111, 100),
            cursor: GRUVBOX_DARK_FOREGROUND,
            st_state: Color::Rgb(131, 165, 152),
            st_model: GRUVBOX_DARK_FOREGROUND,
            st_cost: Color::Rgb(250, 189, 47),
            st_ctx: Color::Rgb(142, 192, 124),
        }
    }

    fn default_light() -> Self {
        Self {
            foreground: GRUVBOX_LIGHT_FOREGROUND,
            background: GRUVBOX_LIGHT_BACKGROUND,
            surface: Color::Rgb(242, 229, 188),
            surface_high: Color::Rgb(235, 219, 178),
            selection: Color::Rgb(213, 196, 161),
            hairline: Color::Rgb(213, 196, 161),
            composer_rule: Color::Rgb(189, 174, 147),
            user_rail: Color::Rgb(66, 123, 88),
            queued_rail: Color::Rgb(189, 174, 147),
            added: Color::Rgb(121, 116, 14),
            removed: Color::Rgb(157, 0, 6),
            changed: Color::Rgb(181, 118, 20),
            added_tint_pct: 28,
            removed_tint_pct: 28,
            changed_tint_pct: 24,
            muted: Color::Rgb(124, 111, 100),
            warning: Color::Rgb(175, 58, 3),
            error: Color::Rgb(157, 0, 6),
            code: Color::Rgb(175, 58, 3),
            user: Color::Rgb(66, 123, 88),
            assistant: GRUVBOX_LIGHT_FOREGROUND,
            tool: Color::Rgb(7, 102, 120),
            gutter: Color::Rgb(146, 131, 116),
            cursor: GRUVBOX_LIGHT_FOREGROUND,
            st_state: Color::Rgb(7, 102, 120),
            st_model: GRUVBOX_LIGHT_FOREGROUND,
            st_cost: Color::Rgb(181, 118, 20),
            st_ctx: Color::Rgb(66, 123, 88),
        }
    }

    /// Warm Ledger design-board reference palette (roles → existing slots).
    fn warm_ledger() -> Self {
        Self {
            foreground: Color::Rgb(0xec, 0xe4, 0xcb),
            background: Color::Rgb(0x26, 0x23, 0x19),
            surface: Color::Rgb(0x1f, 0x1d, 0x15),
            surface_high: Color::Rgb(0x38, 0x31, 0x1c),
            selection: Color::Rgb(0x38, 0x31, 0x1c),
            hairline: Color::Rgb(0x38, 0x34, 0x1f),
            composer_rule: Color::Rgb(0x45, 0x3e, 0x26),
            user_rail: Color::Rgb(0xb3, 0xa6, 0x7e),
            queued_rail: Color::Rgb(0x6b, 0x63, 0x49),
            added: Color::Rgb(0x9d, 0xb8, 0x77),
            removed: Color::Rgb(0xc1, 0x55, 0x3f),
            changed: Color::Rgb(0xd7, 0xa8, 0x3c),
            added_tint_pct: 12,
            removed_tint_pct: 12,
            changed_tint_pct: 10,
            muted: Color::Rgb(0x8b, 0x85, 0x70),
            warning: Color::Rgb(0xd7, 0xa8, 0x3c),
            error: Color::Rgb(0xc1, 0x55, 0x3f),
            code: Color::Rgb(0xd7, 0xa8, 0x3c),
            user: Color::Rgb(0x9d, 0xb8, 0x77),
            assistant: Color::Rgb(0xec, 0xe4, 0xcb),
            tool: Color::Rgb(0x4f, 0x8f, 0x8b),
            gutter: Color::Rgb(0x5f, 0x58, 0x4a),
            cursor: Color::Rgb(0xd7, 0xa8, 0x3c),
            st_state: Color::Rgb(0x4f, 0x8f, 0x8b),
            st_model: Color::Rgb(0xec, 0xe4, 0xcb),
            st_cost: Color::Rgb(0xd7, 0xa8, 0x3c),
            st_ctx: Color::Rgb(0x9d, 0xb8, 0x77),
        }
    }

    fn resolve(self, options: ThemeOptions) -> Palette {
        let background = options.background.resolved(self.background);
        let tint_base = self.background;
        Palette {
            foreground: self.quantize(self.foreground, options),
            background: options.color_level.quantize(background),
            surface: surface_color(background, self.surface, options),
            surface_high: surface_color(background, self.surface_high, options),
            selection: self.quantize(self.selection, options),
            hairline: self.quantize(self.hairline, options),
            composer_rule: self.quantize(self.composer_rule, options),
            user_rail: self.quantize(self.user_rail, options),
            queued_rail: self.quantize(self.queued_rail, options),
            added: self.quantize(self.added, options),
            removed: self.quantize(self.removed, options),
            changed: self.quantize(self.changed, options),
            added_tint: tint(
                tint_base,
                self.added,
                self.added_tint_pct,
                options.color_level,
            ),
            removed_tint: tint(
                tint_base,
                self.removed,
                self.removed_tint_pct,
                options.color_level,
            ),
            changed_tint: tint(
                tint_base,
                self.changed,
                self.changed_tint_pct,
                options.color_level,
            ),
            muted: self.quantize(self.muted, options),
            warning: self.quantize(self.warning, options),
            error: self.quantize(self.error, options),
            code: self.quantize(self.code, options),
            user: self.quantize(self.user, options),
            assistant: self.quantize(self.assistant, options),
            tool: self.quantize(self.tool, options),
            gutter: self.quantize(self.gutter, options),
            cursor: self.quantize(self.cursor, options),
            st_state: self.quantize(self.st_state, options),
            st_model: self.quantize(self.st_model, options),
            st_cost: self.quantize(self.st_cost, options),
            st_ctx: self.quantize(self.st_ctx, options),
        }
    }

    fn quantize(self, color: Color, options: ThemeOptions) -> Color {
        options.color_level.quantize(color)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityTheme {
    pub status: Style,
    pub header: Style,
    pub detail: Style,
    pub gutter: Style,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BannerTheme {
    pub wordmark: Style,
    pub identity: Style,
}

impl BannerTheme {
    fn from_palette(palette: &Palette) -> Self {
        // Brand rule: letterforms are one tone (theme foreground, no
        // color inside the letters); the caption line is dim/comment. Rail
        // colors are brand ANSI slots owned by the banner module, not theme.
        Self {
            wordmark: Style::default().fg(palette.foreground),
            identity: Style::default().fg(palette.muted),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposerTheme {
    pub rule: Style,
    pub queued_rule: Style,
    pub text: Style,
    pub placeholder: Style,
    pub overflow: Style,
    pub token_bar: Style,
}

impl ComposerTheme {
    fn from_palette(palette: &Palette) -> Self {
        Self {
            rule: Style::default().fg(palette.user_rail),
            queued_rule: Style::default().fg(palette.queued_rail),
            text: Style::default().fg(palette.user),
            placeholder: Style::default().fg(palette.muted),
            overflow: Style::default().fg(palette.warning),
            token_bar: Style::default().fg(palette.queued_rail),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusTheme {
    pub base: Style,
    pub state: Style,
    pub model: Style,
    pub cost: Style,
    pub ctx: Style,
    /// Footer v2 (Review v2 §15): the whole footer's default faint token —
    /// reuses the existing `gutter` role (Warm Ledger: `#5f584a`).
    pub faint: Style,
    /// Footer v2 §15: branch parens sit one step brighter than the rest of
    /// the footer — reuses the existing `muted` role (Warm Ledger: `#8b8570`).
    pub branch: Style,
}

impl StatusTheme {
    fn from_palette(palette: &Palette) -> Self {
        Self {
            base: Style::default().fg(palette.foreground),
            state: Style::default().fg(palette.st_state),
            model: Style::default().fg(palette.st_model),
            cost: Style::default().fg(palette.st_cost),
            ctx: Style::default().fg(palette.st_ctx),
            faint: Style::default().fg(palette.gutter),
            branch: Style::default().fg(palette.muted),
        }
    }
}

impl ActivityTheme {
    fn from_palette(palette: &Palette) -> Self {
        Self {
            status: Style::default().fg(palette.foreground),
            header: Style::default()
                .fg(palette.tool)
                .add_modifier(Modifier::BOLD),
            detail: Style::default().fg(palette.foreground),
            gutter: Style::default().fg(palette.gutter),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranscriptTheme {
    pub body: Style,
    pub user: Style,
    pub assistant: Style,
    pub model: Style,
    pub reasoning: Style,
    pub tool: Style,
    pub tool_error: Style,
    pub permission: Style,
    pub patch: Style,
    pub check: Style,
    pub control: Style,
    pub gutter: Style,
    pub hairline: Style,
    pub muted: Style,
    pub added: Style,
    pub removed: Style,
    pub changed: Style,
    pub warning: Style,
    pub error: Style,
    /// Read/reference/companion role (teal rail + companion headers).
    pub companion: Style,
}

impl TranscriptTheme {
    fn from_palette(palette: &Palette) -> Self {
        Self {
            body: Style::default().fg(palette.foreground),
            user: Style::default()
                .fg(palette.user)
                .add_modifier(Modifier::BOLD),
            assistant: Style::default().fg(palette.assistant),
            model: Style::default().fg(palette.muted),
            reasoning: Style::default()
                .fg(palette.muted)
                .add_modifier(Modifier::ITALIC),
            tool: Style::default().fg(palette.tool),
            tool_error: Style::default()
                .fg(palette.error)
                .add_modifier(Modifier::BOLD),
            permission: Style::default()
                .fg(palette.warning)
                .add_modifier(Modifier::BOLD),
            patch: Style::default().fg(palette.changed),
            check: Style::default().fg(palette.tool),
            control: Style::default().fg(palette.muted),
            gutter: Style::default().fg(palette.gutter),
            hairline: Style::default().fg(palette.hairline),
            muted: Style::default().fg(palette.muted),
            added: Style::default().fg(palette.added),
            removed: Style::default().fg(palette.removed),
            changed: Style::default().fg(palette.changed),
            warning: Style::default().fg(palette.warning),
            error: Style::default()
                .fg(palette.error)
                .add_modifier(Modifier::BOLD),
            // Companion/read role reuses the tool/teal hue (semantic read/reference).
            companion: Style::default().fg(palette.tool),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticTheme {
    pub markup: MarkupScopes,
    pub diff: DiffScopes,
    pub syntax: SyntaxScopes,
}

impl SemanticTheme {
    fn from_palette(palette: &Palette) -> Self {
        Self {
            markup: MarkupScopes::from_palette(palette),
            diff: DiffScopes::from_palette(palette),
            syntax: SyntaxScopes::from_palette(palette),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkupScopes {
    pub body: Style,
    pub emphasis: Style,
    pub strong: Style,
    pub code: Style,
    pub link: Style,
    pub inserted: Style,
    pub deleted: Style,
    pub changed: Style,
}

impl MarkupScopes {
    fn from_palette(palette: &Palette) -> Self {
        Self {
            body: Style::default().fg(palette.foreground),
            emphasis: Style::default()
                .fg(palette.foreground)
                .add_modifier(Modifier::ITALIC),
            strong: Style::default()
                .fg(palette.foreground)
                .add_modifier(Modifier::BOLD),
            code: Style::default().fg(palette.tool).bg(palette.surface),
            link: Style::default()
                .fg(palette.tool)
                .add_modifier(Modifier::UNDERLINED),
            inserted: Style::default().fg(palette.added),
            deleted: Style::default().fg(palette.removed),
            changed: Style::default().fg(palette.changed),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffScopes {
    pub inserted: Style,
    pub inserted_body: Style,
    pub deleted: Style,
    pub deleted_body: Style,
    pub changed: Style,
    pub context: Style,
    pub hunk: Style,
}

impl DiffScopes {
    fn from_palette(palette: &Palette) -> Self {
        Self {
            inserted: Style::default().fg(palette.added).bg(palette.added_tint),
            inserted_body: Style::default()
                .fg(palette.foreground)
                .bg(palette.added_tint),
            deleted: Style::default()
                .fg(palette.removed)
                .bg(palette.removed_tint),
            deleted_body: Style::default()
                .fg(palette.muted)
                .bg(palette.removed_tint)
                .add_modifier(Modifier::DIM),
            changed: Style::default()
                .fg(palette.changed)
                .bg(palette.changed_tint),
            context: Style::default().fg(palette.muted),
            hunk: Style::default()
                .fg(palette.gutter)
                .add_modifier(Modifier::ITALIC),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntaxScopes {
    pub plain: Style,
    pub comment: Style,
    pub keyword: Style,
    pub type_name: Style,
    pub function: Style,
    pub string: Style,
    pub number: Style,
    pub constant: Style,
    pub variable: Style,
    pub property: Style,
    pub operator: Style,
    pub punctuation: Style,
    pub macro_name: Style,
    pub attribute: Style,
}

impl SyntaxScopes {
    fn from_palette(palette: &Palette) -> Self {
        Self {
            plain: Style::default().fg(palette.foreground),
            comment: Style::default()
                .fg(palette.gutter)
                .add_modifier(Modifier::ITALIC),
            keyword: Style::default().fg(palette.warning),
            type_name: Style::default().fg(palette.warning),
            function: Style::default().fg(palette.tool),
            string: Style::default().fg(palette.added),
            number: Style::default().fg(palette.added),
            constant: Style::default().fg(palette.added),
            variable: Style::default().fg(palette.foreground),
            property: Style::default().fg(palette.foreground),
            operator: Style::default().fg(palette.warning),
            punctuation: Style::default().fg(palette.foreground),
            macro_name: Style::default().fg(palette.tool),
            attribute: Style::default().fg(palette.warning),
        }
    }

    pub(crate) fn style(&self, kind: SyntaxKind) -> Style {
        match kind {
            SyntaxKind::Plain => self.plain,
            SyntaxKind::Comment => self.comment,
            SyntaxKind::Keyword => self.keyword,
            SyntaxKind::TypeName => self.type_name,
            SyntaxKind::Function => self.function,
            SyntaxKind::String => self.string,
            SyntaxKind::Number => self.number,
            SyntaxKind::Constant => self.constant,
            SyntaxKind::Variable => self.variable,
            SyntaxKind::Property => self.property,
            SyntaxKind::Operator => self.operator,
            SyntaxKind::Punctuation => self.punctuation,
            SyntaxKind::Macro => self.macro_name,
            SyntaxKind::Attribute => self.attribute,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceThemes {
    pub transcript: SurfacePolish,
    pub activity: SurfacePolish,
    pub composer: SurfacePolish,
    pub banner: SurfacePolish,
    pub status: SurfacePolish,
}

impl SurfaceThemes {
    fn from_palette(palette: &Palette) -> Self {
        Self {
            transcript: SurfacePolish::new(palette, palette.background),
            activity: SurfacePolish::new(palette, palette.surface),
            composer: SurfacePolish::new(palette, palette.surface),
            banner: SurfacePolish::new(palette, palette.background),
            status: SurfacePolish::new(palette, palette.surface_high),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfacePolish {
    pub background: Color,
    pub base: Style,
    pub border: Style,
    pub focus: Style,
    pub selection: Style,
}

impl SurfacePolish {
    fn new(palette: &Palette, background: Color) -> Self {
        Self {
            background,
            base: Style::default().fg(palette.foreground).bg(background),
            border: Style::default().fg(palette.gutter).bg(background),
            focus: Style::default()
                .fg(palette.tool)
                .bg(background)
                .add_modifier(Modifier::BOLD),
            selection: Style::default()
                .fg(palette.foreground)
                .bg(palette.selection),
        }
    }
}

fn surface_color(background: Color, fallback: Color, options: ThemeOptions) -> Color {
    if background == Color::Reset {
        Color::Reset
    } else {
        options.color_level.quantize(fallback)
    }
}

fn tint(base: Color, accent: Color, percent: u8, level: ColorLevel) -> Color {
    let (base_red, base_green, base_blue) = color_rgb(base);
    let (accent_red, accent_green, accent_blue) = color_rgb(accent);
    let red = mix_channel(base_red, accent_red, percent);
    let green = mix_channel(base_green, accent_green, percent);
    let blue = mix_channel(base_blue, accent_blue, percent);
    level.quantize(Color::Rgb(red, green, blue))
}

fn mix_channel(base: u8, accent: u8, percent: u8) -> u8 {
    let base = u16::from(base);
    let accent = u16::from(accent);
    let percent = u16::from(percent);
    (((base * (100 - percent)) + (accent * percent)) / 100) as u8
}

fn color_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Black => (0, 0, 0),
        Color::Red => (128, 0, 0),
        Color::Green => (0, 128, 0),
        Color::Yellow => (128, 128, 0),
        Color::Blue => (0, 0, 128),
        Color::Magenta => (128, 0, 128),
        Color::Cyan => (0, 128, 128),
        Color::Gray => (192, 192, 192),
        Color::DarkGray => (128, 128, 128),
        Color::LightRed => (255, 0, 0),
        Color::LightGreen => (0, 255, 0),
        Color::LightYellow => (255, 255, 0),
        Color::LightBlue => (0, 0, 255),
        Color::LightMagenta => (255, 0, 255),
        Color::LightCyan => (0, 255, 255),
        Color::White => (255, 255, 255),
        Color::Indexed(index) => xterm_rgb(index),
        Color::Rgb(red, green, blue) => (red, green, blue),
        Color::Reset => (0, 0, 0),
    }
}

fn nearest_xterm_index(red: u8, green: u8, blue: u8) -> u8 {
    let mut best_index = 0;
    let mut best_distance = u32::MAX;
    for index in 0..=255 {
        let (candidate_red, candidate_green, candidate_blue) = xterm_rgb(index);
        let distance = color_distance(
            (red, green, blue),
            (candidate_red, candidate_green, candidate_blue),
        );
        if distance < best_distance {
            best_index = index;
            best_distance = distance;
        }
    }
    best_index
}

fn nearest_basic_color(red: u8, green: u8, blue: u8) -> Color {
    let mut best_color = Color::Black;
    let mut best_distance = u32::MAX;
    for color in basic_colors() {
        let (candidate_red, candidate_green, candidate_blue) = color_rgb(color);
        let distance = color_distance(
            (red, green, blue),
            (candidate_red, candidate_green, candidate_blue),
        );
        if distance < best_distance {
            best_color = color;
            best_distance = distance;
        }
    }
    best_color
}

fn color_distance(color: (u8, u8, u8), other: (u8, u8, u8)) -> u32 {
    let (red, green, blue) = color;
    let (other_red, other_green, other_blue) = other;
    let red_delta = i32::from(red) - i32::from(other_red);
    let green_delta = i32::from(green) - i32::from(other_green);
    let blue_delta = i32::from(blue) - i32::from(other_blue);
    (red_delta * red_delta + green_delta * green_delta + blue_delta * blue_delta) as u32
}

fn basic_colors() -> [Color; 16] {
    [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ]
}

fn xterm_rgb(index: u8) -> (u8, u8, u8) {
    if index < 16 {
        return color_rgb(basic_colors()[usize::from(index)]);
    }
    if index < 232 {
        return color_cube_rgb(index);
    }
    let shade = 8 + ((index - 232) * 10);
    (shade, shade, shade)
}

fn color_cube_rgb(index: u8) -> (u8, u8, u8) {
    let cube_index = index - 16;
    let red = cube_index / 36;
    let green = (cube_index % 36) / 6;
    let blue = cube_index % 6;
    (cube_channel(red), cube_channel(green), cube_channel(blue))
}

fn cube_channel(value: u8) -> u8 {
    if value == 0 {
        0
    } else {
        55 + (value * 40)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_theme_still_builds() {
        let theme = Theme::default_dark();

        assert_eq!(theme, Theme::default());
        assert_eq!(theme.palette.foreground, Color::Rgb(235, 219, 178));
        assert_eq!(theme.palette.background, GRUVBOX_DARK_BACKGROUND);
        assert_eq!(theme.palette.surface, Color::Rgb(60, 56, 54));
        assert_eq!(theme.palette.surface_high, Color::Rgb(80, 73, 69));
        assert_eq!(theme.palette.hairline, Color::Rgb(80, 73, 69));
        assert_eq!(theme.palette.composer_rule, Color::Rgb(102, 92, 84));
        assert_eq!(theme.palette.user_rail, USER_RAIL_COLOR);
        assert_eq!(theme.palette.queued_rail, Color::Rgb(102, 92, 84));
        assert_eq!(theme.palette.cursor, Color::Rgb(235, 219, 178));
        assert_eq!(theme.palette.added_tint, Color::Rgb(80, 81, 39));
        assert_eq!(theme.palette.removed_tint, Color::Rgb(99, 49, 43));
        assert_eq!(theme.palette.changed_tint, Color::Rgb(90, 75, 41));
        assert_eq!(theme.transcript.added.fg, Some(theme.palette.added));
        assert_eq!(theme.color_level, ColorLevel::TrueColor);
    }

    #[test]
    fn background_modes_resolve_by_theme() {
        let default_order = [
            BackgroundMode::DEFAULT_DARK_OPAQUE,
            BackgroundMode::DEFAULT_DARK_TRANSPARENT,
        ];
        assert_eq!(BackgroundMode::DEFAULT_DARK_BACKGROUNDS, default_order);
        assert_eq!(
            ThemeOptions::default_dark().background,
            BackgroundMode::DEFAULT_DARK_OPAQUE
        );

        let transparent = Theme::default_dark_with(ThemeOptions {
            color_level: ColorLevel::TrueColor,
            background: BackgroundMode::Transparent,
        });
        assert_eq!(transparent.palette.background, Color::Reset);
        assert_eq!(transparent.surfaces.transcript.background, Color::Reset);
        assert_eq!(transparent.surfaces.composer.base.bg, Some(Color::Reset));

        let light = Theme::default_light();
        assert_eq!(
            light.background,
            BackgroundMode::Opaque(GRUVBOX_LIGHT_BACKGROUND)
        );
        assert_eq!(light.palette.background, GRUVBOX_LIGHT_BACKGROUND);
        assert_eq!(light.palette.foreground, Color::Rgb(60, 56, 54));
        assert_eq!(light.palette.cursor, Color::Rgb(60, 56, 54));
        assert_eq!(light.palette.surface, Color::Rgb(242, 229, 188));
        assert_eq!(light.palette.hairline, Color::Rgb(213, 196, 161));
        assert_eq!(light.palette.composer_rule, Color::Rgb(189, 174, 147));
        assert_eq!(light.palette.user_rail, Color::Rgb(66, 123, 88));
        assert_eq!(light.palette.queued_rail, Color::Rgb(189, 174, 147));
        assert_eq!(light.palette.code, Color::Rgb(175, 58, 3));
        assert_eq!(
            light.surfaces.transcript.base.bg,
            Some(GRUVBOX_LIGHT_BACKGROUND)
        );

        let opaque = Theme::default_dark_with(ThemeOptions {
            color_level: ColorLevel::TrueColor,
            background: BackgroundMode::Opaque(Color::Rgb(12, 14, 16)),
        });
        assert_eq!(opaque.palette.background, Color::Rgb(12, 14, 16));
        assert_eq!(
            opaque.surfaces.transcript.base.bg,
            Some(Color::Rgb(12, 14, 16))
        );
        assert!(matches!(
            opaque.surfaces.status.background,
            Color::Rgb(_, _, _)
        ));
    }

    #[test]
    fn warm_ledger_theme_uses_calibrated_tokens_and_tints() {
        let theme = Theme::warm_ledger();

        assert_eq!(theme.palette.hairline, Color::Rgb(0x38, 0x34, 0x1f));
        assert_eq!(theme.palette.composer_rule, Color::Rgb(0x45, 0x3e, 0x26));
        assert_eq!(theme.palette.user_rail, Color::Rgb(0xb3, 0xa6, 0x7e));
        assert_eq!(theme.palette.queued_rail, Color::Rgb(0x6b, 0x63, 0x49));
        assert_eq!(theme.palette.added_tint, Color::Rgb(0x34, 0x34, 0x24));
        assert_eq!(theme.palette.removed_tint, Color::Rgb(0x38, 0x29, 0x1d));
        assert_eq!(theme.palette.changed_tint, Color::Rgb(0x37, 0x30, 0x1c));
        assert_eq!(theme.transcript.hairline.fg, Some(theme.palette.hairline));
        assert_eq!(theme.composer.rule.fg, Some(theme.palette.user_rail));
        assert_eq!(
            theme.composer.queued_rule.fg,
            Some(theme.palette.queued_rail)
        );
        assert_eq!(theme.composer.token_bar.fg, Some(theme.palette.queued_rail));
    }

    #[test]
    fn rgb_colors_quantize_to_indexed_or_basic_levels() {
        let indexed = Theme::default_dark_with(ThemeOptions {
            color_level: ColorLevel::Indexed256,
            background: BackgroundMode::Transparent,
        });
        let basic = Theme::default_dark_with(ThemeOptions {
            color_level: ColorLevel::Basic16,
            background: BackgroundMode::Transparent,
        });

        assert!(matches!(indexed.palette.foreground, Color::Indexed(_)));
        assert!(matches!(indexed.palette.added_tint, Color::Indexed(_)));
        assert!(matches!(
            basic.palette.foreground,
            Color::White | Color::Gray
        ));
        assert!(!matches!(basic.palette.added, Color::Rgb(_, _, _)));
    }

    // #64: deterministic RGB->256 mappings for the quantizer, pinned to
    // known xterm-256 index values so a regression in the cube/greyscale
    // math (or the basic-16 exact-match short-circuit) shows up immediately.
    #[test]
    fn quantizer_maps_known_rgb_values_to_known_xterm_indices() {
        // Exact basic-16 hits win over the 216-cube/greyscale entries that
        // also contain them (both are distance 0; the basic slot sorts
        // first in index order).
        assert_eq!(
            ColorLevel::Indexed256.quantize(Color::Rgb(0, 0, 0)),
            Color::Indexed(0),
            "pure black should hit the basic-16 black slot"
        );
        assert_eq!(
            ColorLevel::Indexed256.quantize(Color::Rgb(255, 0, 0)),
            Color::Indexed(9),
            "pure red should hit the basic-16 bright-red slot"
        );
        // A color-cube-exact RGB triple (steps are 0/95/135/175/215/255) with
        // no basic-16 or greyscale-ramp collision.
        assert_eq!(
            ColorLevel::Indexed256.quantize(Color::Rgb(95, 135, 175)),
            Color::Indexed(67),
            "cube-exact steel-blue should hit xterm index 67"
        );
        // A greyscale-ramp-exact RGB triple with no cube or basic-16 collision.
        assert_eq!(
            ColorLevel::Indexed256.quantize(Color::Rgb(38, 38, 38)),
            Color::Indexed(235),
            "greyscale-ramp-exact grey should hit xterm index 235"
        );
    }

    // #64: TERM_PROGRAM=Apple_Terminal is a known non-supporter and must
    // force the ANSI-256 fallback even with no other signal present.
    #[test]
    fn apple_terminal_forces_indexed_256_fallback() {
        assert_eq!(
            ColorLevel::from_env_signals(None, Some("Apple_Terminal")),
            ColorLevel::Indexed256
        );
    }

    #[test]
    fn colorterm_truecolor_wins_even_over_apple_terminal() {
        assert_eq!(
            ColorLevel::from_env_signals(Some("truecolor"), Some("Apple_Terminal")),
            ColorLevel::TrueColor
        );
        assert_eq!(
            ColorLevel::from_env_signals(Some("24bit"), Some("Apple_Terminal")),
            ColorLevel::TrueColor
        );
    }

    #[test]
    fn unset_signals_default_to_truecolor() {
        assert_eq!(
            ColorLevel::from_env_signals(None, None),
            ColorLevel::TrueColor
        );
        assert_eq!(
            ColorLevel::from_env_signals(None, Some("ghostty")),
            ColorLevel::TrueColor
        );
    }

    #[test]
    fn semantic_scopes_reference_palette_tokens() {
        let dark = Theme::default_dark();
        assert_eq!(dark.scopes.markup.inserted.fg, Some(dark.palette.added));
        assert_eq!(dark.scopes.markup.deleted.fg, Some(dark.palette.removed));
        assert_eq!(dark.scopes.diff.hunk.fg, Some(dark.palette.gutter));
        assert_eq!(dark.scopes.diff.context.fg, Some(dark.palette.muted));
        assert_eq!(dark.scopes.syntax.comment.fg, Some(dark.palette.gutter));

        for theme in [Theme::default_dark(), Theme::default_light()] {
            assert_eq!(theme.scopes.diff.inserted.fg, Some(theme.palette.added));
            assert_eq!(
                theme.scopes.diff.inserted.bg,
                Some(theme.palette.added_tint)
            );
            assert_eq!(
                theme.scopes.diff.inserted_body.fg,
                Some(theme.palette.foreground)
            );
            assert_eq!(
                theme.scopes.diff.inserted_body.bg,
                Some(theme.palette.added_tint)
            );
            assert_eq!(theme.scopes.diff.deleted.fg, Some(theme.palette.removed));
            assert_eq!(
                theme.scopes.diff.deleted.bg,
                Some(theme.palette.removed_tint)
            );
            assert_eq!(theme.scopes.diff.deleted_body.fg, Some(theme.palette.muted));
            assert_eq!(
                theme.scopes.diff.deleted_body.bg,
                Some(theme.palette.removed_tint)
            );
        }
    }

    #[test]
    fn derived_colors_resolve_once_into_palette() {
        let theme = Theme::default_dark_with(ThemeOptions {
            color_level: ColorLevel::TrueColor,
            background: BackgroundMode::Opaque(Color::Rgb(10, 20, 30)),
        });

        assert!(matches!(theme.palette.added_tint, Color::Rgb(_, _, _)));
        assert_eq!(
            theme.scopes.diff.inserted.bg,
            Some(theme.palette.added_tint)
        );
        assert_eq!(
            theme.scopes.diff.deleted.bg,
            Some(theme.palette.removed_tint)
        );
        assert_ne!(theme.palette.added_tint, theme.palette.added);
    }
}
