use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU8, Ordering},
    time::Duration,
};

use color_eyre::eyre::{Result, WrapErr};
use crossterm::event::{Event, MouseEvent, MouseEventKind};
use medusa_core::permissions::{PermissionMode, PermissionPolicy};
use medusa_core::persistence::atomic_write_private;
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use crate::constants::{BELL_MIN_WORKING_DURATION, DEFAULT_MODEL_CHOICES};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AppSettings {
    #[serde(default)]
    pub(crate) theme: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) permission_mode: Option<String>,
    #[serde(default)]
    pub(crate) bell: Option<bool>,
    #[serde(default)]
    pub(crate) reasoning_effort: Option<String>,
}

impl AppSettings {
    pub(crate) fn theme(&self) -> Option<ThemeKind> {
        self.theme.as_deref().and_then(ThemeKind::from_name)
    }

    pub(crate) fn model(&self) -> Option<String> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(ToString::to_string)
    }

    pub(crate) fn reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort
            .as_deref()
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
            .map(ToString::to_string)
    }

    pub(crate) fn permission_mode(&self) -> PermissionMode {
        self.permission_mode
            .as_deref()
            .and_then(PermissionMode::from_name)
            .unwrap_or(PermissionMode::Guarded)
    }
}

pub(crate) fn app_settings_path(workspace: &Path) -> PathBuf {
    workspace.join(".medusa").join("settings.json")
}

pub(crate) fn load_app_settings(workspace: &Path) -> Result<AppSettings> {
    let path = app_settings_path(workspace);
    if !path.exists() {
        return Ok(AppSettings::default());
    }

    let text =
        fs::read_to_string(&path).wrap_err_with(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).wrap_err_with(|| format!("failed to parse {}", path.display()))
}

pub(crate) fn save_app_settings(workspace: &Path, settings: &AppSettings) -> Result<()> {
    let path = app_settings_path(workspace);
    let json = serde_json::to_string_pretty(settings).wrap_err("failed to encode settings")?;
    atomic_write_private(&path, json)
        .wrap_err_with(|| format!("failed to write {}", path.display()))
}

pub(crate) fn save_theme_preference(workspace: &Path, theme: ThemeKind) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.theme = Some(theme.name().to_string());
    save_app_settings(workspace, &settings)
}

pub(crate) fn save_model_preference(workspace: &Path, model: &str) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.model = Some(model.trim().to_string());
    save_app_settings(workspace, &settings)
}

pub(crate) fn save_reasoning_preference(workspace: &Path, effort: &str) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.reasoning_effort = Some(effort.trim().to_string());
    save_app_settings(workspace, &settings)
}

pub(crate) fn save_model_picker_preferences(
    workspace: &Path,
    model: &str,
    effort: &str,
) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.model = Some(model.trim().to_string());
    settings.reasoning_effort = Some(effort.trim().to_string());
    save_app_settings(workspace, &settings)
}

pub(crate) fn save_permission_mode_preference(
    workspace: &Path,
    mode: PermissionMode,
) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.permission_mode = Some(mode.name().to_string());
    save_app_settings(workspace, &settings)?;
    PermissionPolicy::write_mode(workspace, mode)
}

pub(crate) fn save_bell_preference(workspace: &Path, enabled: bool) -> Result<()> {
    let mut settings = load_app_settings(workspace).unwrap_or_default();
    settings.bell = Some(enabled);
    save_app_settings(workspace, &settings)
}

/// Effective bell enablement: the MEDUSA_BELL environment variable overrides
/// the workspace setting ("off"/"0"/"false"/"no" disables, "on"/"1"/"true"/
/// "yes" enables, anything else falls back to the setting).
pub(crate) fn bell_enabled(setting: bool, env_value: Option<&str>) -> bool {
    match env_value.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if matches!(value.as_str(), "off" | "0" | "false" | "no") => false,
        Some(value) if matches!(value.as_str(), "on" | "1" | "true" | "yes") => true,
        _ => setting,
    }
}

/// Bell gating: only ring for turns that ran long enough that the user has
/// plausibly tabbed away — rapid turns should never ding.
pub(crate) fn should_ring_bell(enabled: bool, working_for: Option<Duration>) -> bool {
    enabled && working_for.is_some_and(|elapsed| elapsed > BELL_MIN_WORKING_DURATION)
}

pub(crate) static ACTIVE_THEME: AtomicU8 = AtomicU8::new(0);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemeKind {
    Medusa = 0,
    OpenCode = 1,
    TokyoNight = 2,
    Catppuccin = 3,
    Dracula = 4,
    Nord = 5,
    Gruvbox = 6,
    SolarizedDark = 7,
    MaterialDark = 8,
    MaterialTeal = 9,
    MaterialAmber = 10,
    MaterialIndigo = 11,
    MaterialRose = 12,
    RosePine = 13,
    AyuMirage = 14,
    Everforest = 15,
    Vesper = 16,
}

pub(crate) const THEME_KINDS: [ThemeKind; 17] = [
    ThemeKind::Medusa,
    ThemeKind::OpenCode,
    ThemeKind::TokyoNight,
    ThemeKind::Catppuccin,
    ThemeKind::Dracula,
    ThemeKind::Nord,
    ThemeKind::Gruvbox,
    ThemeKind::SolarizedDark,
    ThemeKind::MaterialDark,
    ThemeKind::MaterialTeal,
    ThemeKind::MaterialAmber,
    ThemeKind::MaterialIndigo,
    ThemeKind::MaterialRose,
    ThemeKind::RosePine,
    ThemeKind::AyuMirage,
    ThemeKind::Everforest,
    ThemeKind::Vesper,
];

#[derive(Debug, Clone, Copy)]
pub(crate) struct ThemePalette {
    pub(crate) text: Color,
    pub(crate) muted: Color,
    pub(crate) accent: Color,
    pub(crate) prompt: Color,
    pub(crate) separator: Color,
    pub(crate) selected_fg: Color,
    pub(crate) selected_bg: Color,
    pub(crate) activity_bg: Color,
    pub(crate) user_bg: Color,
    pub(crate) success: Color,
    pub(crate) error: Color,
    pub(crate) info: Color,
    pub(crate) tool: Color,
    pub(crate) quote: Color,
    pub(crate) code_fg: Color,
    pub(crate) code_bg: Color,
    pub(crate) inline_code_fg: Color,
    pub(crate) inline_code_bg: Color,
}

pub(crate) const MATERIAL_RED_400: Color = Color::Rgb(239, 83, 80);
pub(crate) const MATERIAL_PINK_300: Color = Color::Rgb(240, 98, 146);
pub(crate) const MATERIAL_PINK_200: Color = Color::Rgb(244, 143, 177);
pub(crate) const MATERIAL_DEEP_PURPLE_300: Color = Color::Rgb(149, 117, 205);
pub(crate) const MATERIAL_INDIGO_300: Color = Color::Rgb(121, 134, 203);
pub(crate) const MATERIAL_LIGHT_BLUE_300: Color = Color::Rgb(79, 195, 247);
pub(crate) const MATERIAL_CYAN_300: Color = Color::Rgb(77, 208, 225);
pub(crate) const MATERIAL_TEAL_200: Color = Color::Rgb(128, 203, 196);
pub(crate) const MATERIAL_TEAL_300: Color = Color::Rgb(77, 182, 172);
pub(crate) const MATERIAL_TEAL_400: Color = Color::Rgb(38, 166, 154);
pub(crate) const MATERIAL_GREEN_400: Color = Color::Rgb(102, 187, 106);
pub(crate) const MATERIAL_AMBER_300: Color = Color::Rgb(255, 213, 79);
pub(crate) const MATERIAL_AMBER_400: Color = Color::Rgb(255, 202, 40);
pub(crate) const MATERIAL_ORANGE_300: Color = Color::Rgb(255, 183, 77);
pub(crate) const MATERIAL_BLUE_GREY_50: Color = Color::Rgb(236, 239, 241);
pub(crate) const MATERIAL_BLUE_GREY_100: Color = Color::Rgb(207, 216, 220);
pub(crate) const MATERIAL_BLUE_GREY_200: Color = Color::Rgb(176, 190, 197);
pub(crate) const MATERIAL_BLUE_GREY_800: Color = Color::Rgb(55, 71, 79);
pub(crate) const MATERIAL_BLUE_GREY_900: Color = Color::Rgb(38, 50, 56);

pub(crate) fn material_dark_palette(
    accent: Color,
    prompt: Color,
    tool: Color,
    inline_code_fg: Color,
) -> ThemePalette {
    ThemePalette {
        text: MATERIAL_BLUE_GREY_50,
        muted: MATERIAL_BLUE_GREY_200,
        accent,
        prompt,
        separator: MATERIAL_BLUE_GREY_800,
        selected_fg: Color::Rgb(12, 18, 22),
        selected_bg: accent,
        activity_bg: MATERIAL_BLUE_GREY_900,
        user_bg: Color::Rgb(45, 35, 20),
        success: MATERIAL_GREEN_400,
        error: MATERIAL_RED_400,
        info: MATERIAL_CYAN_300,
        tool,
        quote: MATERIAL_BLUE_GREY_100,
        code_fg: MATERIAL_BLUE_GREY_50,
        code_bg: MATERIAL_BLUE_GREY_900,
        inline_code_fg,
        inline_code_bg: Color::Rgb(18, 31, 35),
    }
}

impl ThemeKind {
    pub(crate) fn from_workspace_settings(workspace: &Path) -> Self {
        Self::resolve(env::var("MEDUSA_THEME").ok().as_deref(), workspace)
    }

    /// Resolve the active theme from an explicit `MEDUSA_THEME`-style override
    /// (highest priority) then the persisted workspace settings. Taking the
    /// override as a parameter keeps tests off the process-global environment:
    /// `set_var` racing the parallel test harness's `getenv`-backed readers is
    /// undefined behavior.
    pub(crate) fn resolve(env_override: Option<&str>, workspace: &Path) -> Self {
        env_override
            .and_then(Self::from_name)
            .or_else(|| {
                load_app_settings(workspace)
                    .ok()
                    .and_then(|settings| settings.theme())
            })
            .unwrap_or(Self::Medusa)
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        let normalized = name.trim().to_ascii_lowercase().replace(['_', ' '], "-");

        match normalized.as_str() {
            "medusa" | "default" => Some(Self::Medusa),
            "opencode" | "open-code" => Some(Self::OpenCode),
            "tokyonight" | "tokyo-night" => Some(Self::TokyoNight),
            "catppuccin" | "mocha" => Some(Self::Catppuccin),
            "dracula" => Some(Self::Dracula),
            "nord" => Some(Self::Nord),
            "gruvbox" | "gruvbox-dark" => Some(Self::Gruvbox),
            "solarized" | "solarized-dark" => Some(Self::SolarizedDark),
            "material" | "material-dark" => Some(Self::MaterialDark),
            "material-teal" | "material-cyan" => Some(Self::MaterialTeal),
            "material-amber" | "material-yellow" => Some(Self::MaterialAmber),
            "material-indigo" | "material-purple" => Some(Self::MaterialIndigo),
            "material-rose" | "material-pink" => Some(Self::MaterialRose),
            "rose-pine" | "rosepine" => Some(Self::RosePine),
            "ayu" | "ayu-mirage" => Some(Self::AyuMirage),
            "everforest" | "everforest-dark" => Some(Self::Everforest),
            "vesper" => Some(Self::Vesper),
            _ => None,
        }
    }

    pub(crate) fn all() -> &'static [Self] {
        &THEME_KINDS
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Medusa => "medusa",
            Self::OpenCode => "opencode",
            Self::TokyoNight => "tokyonight",
            Self::Catppuccin => "catppuccin",
            Self::Dracula => "dracula",
            Self::Nord => "nord",
            Self::Gruvbox => "gruvbox",
            Self::SolarizedDark => "solarized-dark",
            Self::MaterialDark => "material-dark",
            Self::MaterialTeal => "material-teal",
            Self::MaterialAmber => "material-amber",
            Self::MaterialIndigo => "material-indigo",
            Self::MaterialRose => "material-rose",
            Self::RosePine => "rose-pine",
            Self::AyuMirage => "ayu-mirage",
            Self::Everforest => "everforest",
            Self::Vesper => "vesper",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Medusa => "Medusa",
            Self::OpenCode => "OpenCode",
            Self::TokyoNight => "Tokyo Night",
            Self::Catppuccin => "Catppuccin",
            Self::Dracula => "Dracula",
            Self::Nord => "Nord",
            Self::Gruvbox => "Gruvbox",
            Self::SolarizedDark => "Solarized Dark",
            Self::MaterialDark => "Material Dark",
            Self::MaterialTeal => "Material Teal",
            Self::MaterialAmber => "Material Amber",
            Self::MaterialIndigo => "Material Indigo",
            Self::MaterialRose => "Material Rose",
            Self::RosePine => "Rosé Pine",
            Self::AyuMirage => "Ayu Mirage",
            Self::Everforest => "Everforest",
            Self::Vesper => "Vesper",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::Medusa => "sharp black, acid green, warm prompt accents",
            Self::OpenCode => "quiet blue command surface with crisp contrast",
            Self::TokyoNight => "deep navy with cyan highlights",
            Self::Catppuccin => "soft mocha surface with rosewater accents",
            Self::Dracula => "inky violet with neon pink and green highlights",
            Self::Nord => "arctic blue-gray calm with frosty cyan accents",
            Self::Gruvbox => "retro warm earth tones with punchy orange prompts",
            Self::SolarizedDark => "low-glare teal base with balanced amber accents",
            Self::MaterialDark => "blue-grey Material base with balanced teal and amber",
            Self::MaterialTeal => "Material teal command surface with cyan tool accents",
            Self::MaterialAmber => "Material amber selection with teal prompts",
            Self::MaterialIndigo => "Material indigo focus with light-blue tooling",
            Self::MaterialRose => "Material rose accents with teal supporting signals",
            Self::RosePine => "muted rose and gold over a soho-night violet base",
            Self::AyuMirage => "dusky slate with warm orange and sky-blue accents",
            Self::Everforest => "soft forest greens with warm bark and sage tones",
            Self::Vesper => "near-black minimalism with a single peach accent",
        }
    }

    pub(crate) fn palette(self) -> ThemePalette {
        match self {
            Self::Medusa => ThemePalette {
                text: Color::Rgb(216, 216, 220),
                muted: Color::Rgb(132, 132, 142),
                accent: Color::Rgb(84, 214, 147),
                prompt: Color::Rgb(228, 169, 104),
                separator: Color::Rgb(35, 38, 42),
                selected_fg: Color::Rgb(10, 12, 14),
                selected_bg: Color::Rgb(84, 214, 147),
                activity_bg: Color::Rgb(18, 24, 30),
                user_bg: Color::Rgb(38, 30, 22),
                success: Color::Rgb(84, 214, 147),
                error: Color::Rgb(230, 111, 125),
                info: Color::Rgb(126, 176, 255),
                tool: Color::Rgb(126, 176, 255),
                quote: Color::Rgb(168, 176, 188),
                code_fg: Color::Rgb(190, 205, 220),
                code_bg: Color::Rgb(16, 18, 22),
                inline_code_fg: Color::Rgb(147, 210, 178),
                inline_code_bg: Color::Rgb(20, 26, 24),
            },
            Self::OpenCode => ThemePalette {
                text: Color::Rgb(221, 224, 229),
                muted: Color::Rgb(132, 139, 148),
                accent: Color::Rgb(96, 165, 250),
                prompt: Color::Rgb(245, 158, 11),
                separator: Color::Rgb(42, 46, 54),
                selected_fg: Color::Rgb(7, 10, 15),
                selected_bg: Color::Rgb(96, 165, 250),
                activity_bg: Color::Rgb(18, 27, 40),
                user_bg: Color::Rgb(42, 32, 18),
                success: Color::Rgb(52, 211, 153),
                error: Color::Rgb(248, 113, 113),
                info: Color::Rgb(147, 197, 253),
                tool: Color::Rgb(147, 197, 253),
                quote: Color::Rgb(176, 184, 196),
                code_fg: Color::Rgb(205, 213, 224),
                code_bg: Color::Rgb(17, 21, 28),
                inline_code_fg: Color::Rgb(191, 219, 254),
                inline_code_bg: Color::Rgb(23, 31, 44),
            },
            Self::TokyoNight => ThemePalette {
                text: Color::Rgb(192, 202, 245),
                muted: Color::Rgb(122, 162, 247),
                accent: Color::Rgb(125, 207, 255),
                prompt: Color::Rgb(255, 158, 100),
                separator: Color::Rgb(59, 66, 97),
                selected_fg: Color::Rgb(26, 27, 38),
                selected_bg: Color::Rgb(125, 207, 255),
                activity_bg: Color::Rgb(36, 40, 59),
                user_bg: Color::Rgb(49, 38, 36),
                success: Color::Rgb(158, 206, 106),
                error: Color::Rgb(247, 118, 142),
                info: Color::Rgb(125, 207, 255),
                tool: Color::Rgb(125, 207, 255),
                quote: Color::Rgb(154, 165, 206),
                code_fg: Color::Rgb(192, 202, 245),
                code_bg: Color::Rgb(22, 22, 30),
                inline_code_fg: Color::Rgb(187, 154, 247),
                inline_code_bg: Color::Rgb(38, 35, 58),
            },
            Self::Catppuccin => ThemePalette {
                text: Color::Rgb(205, 214, 244),
                muted: Color::Rgb(166, 173, 200),
                accent: Color::Rgb(137, 220, 235),
                prompt: Color::Rgb(250, 179, 135),
                separator: Color::Rgb(69, 71, 90),
                selected_fg: Color::Rgb(17, 17, 27),
                selected_bg: Color::Rgb(137, 220, 235),
                activity_bg: Color::Rgb(30, 30, 46),
                user_bg: Color::Rgb(51, 39, 39),
                success: Color::Rgb(166, 227, 161),
                error: Color::Rgb(243, 139, 168),
                info: Color::Rgb(137, 180, 250),
                tool: Color::Rgb(137, 180, 250),
                quote: Color::Rgb(180, 190, 254),
                code_fg: Color::Rgb(203, 214, 244),
                code_bg: Color::Rgb(24, 24, 37),
                inline_code_fg: Color::Rgb(148, 226, 213),
                inline_code_bg: Color::Rgb(30, 30, 46),
            },
            Self::Dracula => ThemePalette {
                text: Color::Rgb(248, 248, 242),
                muted: Color::Rgb(139, 143, 173),
                accent: Color::Rgb(189, 147, 249),
                prompt: Color::Rgb(255, 184, 108),
                separator: Color::Rgb(68, 71, 90),
                selected_fg: Color::Rgb(40, 42, 54),
                selected_bg: Color::Rgb(189, 147, 249),
                activity_bg: Color::Rgb(40, 42, 54),
                user_bg: Color::Rgb(50, 43, 38),
                success: Color::Rgb(80, 250, 123),
                error: Color::Rgb(255, 85, 85),
                info: Color::Rgb(139, 233, 253),
                tool: Color::Rgb(139, 233, 253),
                quote: Color::Rgb(241, 250, 140),
                code_fg: Color::Rgb(248, 248, 242),
                code_bg: Color::Rgb(33, 34, 44),
                inline_code_fg: Color::Rgb(255, 121, 198),
                inline_code_bg: Color::Rgb(48, 42, 65),
            },
            Self::Nord => ThemePalette {
                text: Color::Rgb(216, 222, 233),
                muted: Color::Rgb(129, 161, 193),
                accent: Color::Rgb(136, 192, 208),
                prompt: Color::Rgb(235, 203, 139),
                separator: Color::Rgb(67, 76, 94),
                selected_fg: Color::Rgb(46, 52, 64),
                selected_bg: Color::Rgb(136, 192, 208),
                activity_bg: Color::Rgb(59, 66, 82),
                user_bg: Color::Rgb(70, 61, 48),
                success: Color::Rgb(163, 190, 140),
                error: Color::Rgb(191, 97, 106),
                info: Color::Rgb(129, 161, 193),
                tool: Color::Rgb(129, 161, 193),
                quote: Color::Rgb(180, 142, 173),
                code_fg: Color::Rgb(229, 233, 240),
                code_bg: Color::Rgb(36, 42, 54),
                inline_code_fg: Color::Rgb(143, 188, 187),
                inline_code_bg: Color::Rgb(48, 56, 70),
            },
            Self::Gruvbox => ThemePalette {
                text: Color::Rgb(235, 219, 178),
                muted: Color::Rgb(168, 153, 132),
                accent: Color::Rgb(184, 187, 38),
                prompt: Color::Rgb(254, 128, 25),
                separator: Color::Rgb(80, 73, 69),
                selected_fg: Color::Rgb(40, 40, 40),
                selected_bg: Color::Rgb(250, 189, 47),
                activity_bg: Color::Rgb(60, 56, 54),
                user_bg: Color::Rgb(66, 49, 35),
                success: Color::Rgb(184, 187, 38),
                error: Color::Rgb(251, 73, 52),
                info: Color::Rgb(131, 165, 152),
                tool: Color::Rgb(131, 165, 152),
                quote: Color::Rgb(211, 134, 155),
                code_fg: Color::Rgb(235, 219, 178),
                code_bg: Color::Rgb(29, 32, 33),
                inline_code_fg: Color::Rgb(250, 189, 47),
                inline_code_bg: Color::Rgb(50, 48, 47),
            },
            Self::SolarizedDark => ThemePalette {
                text: Color::Rgb(131, 148, 150),
                muted: Color::Rgb(88, 110, 117),
                accent: Color::Rgb(42, 161, 152),
                prompt: Color::Rgb(181, 137, 0),
                separator: Color::Rgb(7, 54, 66),
                selected_fg: Color::Rgb(0, 43, 54),
                selected_bg: Color::Rgb(42, 161, 152),
                activity_bg: Color::Rgb(7, 54, 66),
                user_bg: Color::Rgb(58, 49, 15),
                success: Color::Rgb(133, 153, 0),
                error: Color::Rgb(220, 50, 47),
                info: Color::Rgb(38, 139, 210),
                tool: Color::Rgb(38, 139, 210),
                quote: Color::Rgb(108, 113, 196),
                code_fg: Color::Rgb(147, 161, 161),
                code_bg: Color::Rgb(0, 35, 44),
                inline_code_fg: Color::Rgb(203, 75, 22),
                inline_code_bg: Color::Rgb(7, 54, 66),
            },
            Self::MaterialDark => material_dark_palette(
                MATERIAL_TEAL_300,
                MATERIAL_AMBER_400,
                MATERIAL_CYAN_300,
                MATERIAL_TEAL_200,
            ),
            Self::MaterialTeal => material_dark_palette(
                MATERIAL_TEAL_400,
                MATERIAL_ORANGE_300,
                MATERIAL_CYAN_300,
                MATERIAL_TEAL_200,
            ),
            Self::MaterialAmber => material_dark_palette(
                MATERIAL_AMBER_400,
                MATERIAL_TEAL_300,
                MATERIAL_ORANGE_300,
                MATERIAL_AMBER_300,
            ),
            Self::MaterialIndigo => material_dark_palette(
                MATERIAL_INDIGO_300,
                MATERIAL_AMBER_300,
                MATERIAL_LIGHT_BLUE_300,
                MATERIAL_DEEP_PURPLE_300,
            ),
            Self::MaterialRose => material_dark_palette(
                MATERIAL_PINK_300,
                MATERIAL_AMBER_300,
                MATERIAL_TEAL_200,
                MATERIAL_PINK_200,
            ),
            Self::RosePine => ThemePalette {
                text: Color::Rgb(224, 222, 244),
                muted: Color::Rgb(144, 140, 170),
                accent: Color::Rgb(235, 188, 186),
                prompt: Color::Rgb(246, 193, 119),
                separator: Color::Rgb(38, 35, 58),
                selected_fg: Color::Rgb(25, 23, 36),
                selected_bg: Color::Rgb(235, 188, 186),
                activity_bg: Color::Rgb(31, 29, 46),
                user_bg: Color::Rgb(42, 33, 24),
                success: Color::Rgb(156, 207, 216),
                error: Color::Rgb(235, 111, 146),
                info: Color::Rgb(196, 167, 231),
                tool: Color::Rgb(156, 207, 216),
                quote: Color::Rgb(184, 179, 209),
                code_fg: Color::Rgb(224, 222, 244),
                code_bg: Color::Rgb(31, 29, 46),
                inline_code_fg: Color::Rgb(196, 167, 231),
                inline_code_bg: Color::Rgb(38, 35, 58),
            },
            Self::AyuMirage => ThemePalette {
                text: Color::Rgb(203, 204, 198),
                muted: Color::Rgb(112, 122, 140),
                accent: Color::Rgb(115, 208, 255),
                prompt: Color::Rgb(255, 167, 89),
                separator: Color::Rgb(51, 65, 94),
                selected_fg: Color::Rgb(31, 36, 48),
                selected_bg: Color::Rgb(115, 208, 255),
                activity_bg: Color::Rgb(35, 40, 52),
                user_bg: Color::Rgb(48, 38, 24),
                success: Color::Rgb(186, 230, 126),
                error: Color::Rgb(255, 102, 102),
                info: Color::Rgb(92, 207, 230),
                tool: Color::Rgb(92, 207, 230),
                quote: Color::Rgb(166, 172, 205),
                code_fg: Color::Rgb(203, 204, 198),
                code_bg: Color::Rgb(36, 41, 54),
                inline_code_fg: Color::Rgb(149, 230, 203),
                inline_code_bg: Color::Rgb(42, 48, 62),
            },
            Self::Everforest => ThemePalette {
                text: Color::Rgb(211, 198, 170),
                muted: Color::Rgb(133, 146, 137),
                accent: Color::Rgb(167, 192, 128),
                prompt: Color::Rgb(230, 152, 117),
                separator: Color::Rgb(71, 82, 88),
                selected_fg: Color::Rgb(45, 53, 59),
                selected_bg: Color::Rgb(167, 192, 128),
                activity_bg: Color::Rgb(52, 63, 68),
                user_bg: Color::Rgb(58, 49, 37),
                success: Color::Rgb(167, 192, 128),
                error: Color::Rgb(230, 126, 128),
                info: Color::Rgb(127, 187, 179),
                tool: Color::Rgb(127, 187, 179),
                quote: Color::Rgb(157, 169, 160),
                code_fg: Color::Rgb(211, 198, 170),
                code_bg: Color::Rgb(39, 46, 51),
                inline_code_fg: Color::Rgb(131, 192, 146),
                inline_code_bg: Color::Rgb(47, 56, 62),
            },
            Self::Vesper => ThemePalette {
                text: Color::Rgb(209, 209, 209),
                muted: Color::Rgb(118, 118, 118),
                accent: Color::Rgb(255, 199, 153),
                prompt: Color::Rgb(255, 199, 153),
                separator: Color::Rgb(40, 40, 40),
                selected_fg: Color::Rgb(16, 16, 16),
                selected_bg: Color::Rgb(255, 199, 153),
                activity_bg: Color::Rgb(24, 24, 24),
                user_bg: Color::Rgb(38, 30, 22),
                success: Color::Rgb(153, 255, 228),
                error: Color::Rgb(255, 128, 128),
                info: Color::Rgb(153, 255, 228),
                tool: Color::Rgb(172, 172, 172),
                quote: Color::Rgb(160, 160, 160),
                code_fg: Color::Rgb(209, 209, 209),
                code_bg: Color::Rgb(20, 20, 20),
                inline_code_fg: Color::Rgb(255, 199, 153),
                inline_code_bg: Color::Rgb(30, 30, 30),
            },
        }
    }
}

pub(crate) fn set_active_theme(theme: ThemeKind) {
    ACTIVE_THEME.store(theme as u8, Ordering::Relaxed);
}

pub(crate) fn theme_index(theme: ThemeKind) -> usize {
    ThemeKind::all()
        .iter()
        .position(|candidate| *candidate == theme)
        .unwrap_or(0)
}

pub(crate) fn theme_at_offset(theme: ThemeKind, offset: isize) -> ThemeKind {
    let themes = ThemeKind::all();
    let next = (theme_index(theme) as isize + offset).rem_euclid(themes.len() as isize) as usize;
    themes[next]
}

/// Selectable model slugs. Primary source is Codex's own backend model cache
/// (`~/.codex/models_cache.json`), so the picker reflects exactly what the
/// account can use — new models appear with no code change. Falls back to a
/// built-in list when the cache is absent (non-Codex provider / fresh install).
/// The current model is always present, pinned first if the source omits it.
pub(crate) fn model_choices(current: &str) -> Vec<String> {
    let mut choices = medusa_core::models::codex_backend_models()
        .map(|models| {
            models
                .into_iter()
                .map(|model| model.slug)
                .collect::<Vec<_>>()
        })
        .filter(|slugs: &Vec<String>| !slugs.is_empty())
        .unwrap_or_else(|| {
            DEFAULT_MODEL_CHOICES
                .iter()
                .map(|model| (*model).to_string())
                .collect()
        });
    let current = current.trim();
    if !current.is_empty() && !choices.iter().any(|model| model == current) {
        choices.insert(0, current.to_string());
    }
    choices
}

/// Display label + optional description for a model slug, from the Codex
/// backend cache. Unknown slugs (custom/env-set models) render as the slug.
pub(crate) fn model_display(slug: &str) -> (String, Option<String>) {
    medusa_core::models::codex_backend_models()
        .and_then(|models| models.into_iter().find(|model| model.slug == slug))
        .map(|model| (model.display_name, model.description))
        .unwrap_or_else(|| (slug.to_string(), None))
}

pub(crate) fn model_index(current: &str) -> usize {
    model_choices(current)
        .iter()
        .position(|model| model == current)
        .unwrap_or(0)
}

pub(crate) fn model_default_reasoning(model: &str) -> Option<String> {
    medusa_core::models::codex_backend_models()
        .and_then(|models| models.into_iter().find(|candidate| candidate.slug == model))
        .and_then(|model| model.default_reasoning)
}

/// Reasoning efforts selectable for `model`: the backend's per-model list when
/// known, else standard defaults. The active effort is always present.
pub(crate) fn reasoning_choices(model: &str, current: &str) -> Vec<String> {
    let mut choices = medusa_core::models::reasoning_efforts_for(model)
        .into_iter()
        .map(|level| level.effort)
        .collect::<Vec<_>>();
    let current = current.trim();
    if !current.is_empty() && !choices.iter().any(|effort| effort == current) {
        choices.push(current.to_string());
    }
    choices
}

pub(crate) fn reasoning_index(model: &str, current: &str) -> usize {
    reasoning_choices(model, current)
        .iter()
        .position(|effort| effort == current)
        .unwrap_or(0)
}

pub(crate) fn preferred_reasoning_for_model(model: &str, preferred: &str) -> String {
    let choices = reasoning_choices(model, "");
    if choices.iter().any(|effort| effort == preferred) {
        return preferred.to_string();
    }
    if let Some(default) = model_default_reasoning(model)
        && choices.iter().any(|effort| effort == &default)
    {
        return default;
    }
    choices
        .iter()
        .find(|effort| effort.as_str() == "medium")
        .or_else(|| choices.first())
        .cloned()
        .unwrap_or_else(|| "medium".to_string())
}

/// Backend description for a reasoning effort of a model, when the cache has one.
pub(crate) fn reasoning_description(model: &str, effort: &str) -> Option<String> {
    medusa_core::models::reasoning_efforts_for(model)
        .into_iter()
        .find(|level| level.effort == effort)
        .and_then(|level| level.description)
}

pub(crate) fn permission_mode_index(mode: PermissionMode) -> usize {
    PermissionMode::all()
        .iter()
        .position(|candidate| *candidate == mode)
        .unwrap_or(0)
}

pub(crate) fn active_theme() -> ThemeKind {
    match ACTIVE_THEME.load(Ordering::Relaxed) {
        1 => ThemeKind::OpenCode,
        2 => ThemeKind::TokyoNight,
        3 => ThemeKind::Catppuccin,
        4 => ThemeKind::Dracula,
        5 => ThemeKind::Nord,
        6 => ThemeKind::Gruvbox,
        7 => ThemeKind::SolarizedDark,
        8 => ThemeKind::MaterialDark,
        9 => ThemeKind::MaterialTeal,
        10 => ThemeKind::MaterialAmber,
        11 => ThemeKind::MaterialIndigo,
        12 => ThemeKind::MaterialRose,
        13 => ThemeKind::RosePine,
        14 => ThemeKind::AyuMirage,
        15 => ThemeKind::Everforest,
        16 => ThemeKind::Vesper,
        _ => ThemeKind::Medusa,
    }
}

pub(crate) fn palette() -> ThemePalette {
    active_theme().palette()
}

pub(crate) fn event_requests_immediate_draw(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp | MouseEventKind::ScrollDown,
            ..
        })
    )
}
