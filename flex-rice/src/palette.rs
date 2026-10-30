//! Theme palette bridge: the rice's active colours, mapped onto Material 3
//! semantic color roles and an engine [`Theme`].
//!
//! `flex-core` ships the three upstream themes (`default`, `nocolor`,
//! `plain`) and stays machine-agnostic, so it cannot know about this
//! machine's colour scheme. This module reads the *active* theme written by
//! `flex-theme activate` into a color map, resolves standard Material 3
//! [`ColorScheme`] roles, and derives the engine's tokens so `-t default`
//! renders in the same colours as Hyprland, Waybar and Kitty.
//!
//! Resolution order (first readable wins):
//!
//! 1. `$FLEX_THEME_FILE` — an explicit `definition.json` or `theme.css`
//!    (the test seam; when set it is authoritative and never falls through).
//! 2. `~/.config/themes/current/definition.json`.
//! 3. `~/.config/themes/current/theme.css`.
//!
//! A missing, unreadable or malformed file yields `None`, and the caller keeps
//! [`Theme::DEFAULT`] — so a checkout without the rice still renders.
//!
//! Only `-t default` is themed. `nocolor` and `plain` are explicit requests
//! for a colorless menu and never consult the palette.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use flex_core::theme::ColorScheme;
use flex_core::Theme;
use ratatui::style::Color;
use serde::Deserialize;

/// Key-value dictionary of theme colors mapped from JSON or CSS definitions.
///
/// Decoupled from specific theme conventions: semantic UI roles (accent,
/// borders, gauges, surfaces) are resolved into a Material 3 [`ColorScheme`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Palette {
    colors: HashMap<String, Color>,
}

impl Palette {
    /// Read a palette from `path`, choosing the parser by extension.
    ///
    /// `.json` is parsed as a `definition.json`; anything else is parsed as
    /// the `@define-color` form used by `theme.css`.
    #[must_use]
    pub fn from_path(path: &Path) -> Option<Self> {
        let src = std::fs::read_to_string(path).ok()?;
        if path.extension().is_some_and(|ext| ext == "json") {
            Self::from_json(&src)
        } else {
            Self::from_css(&src)
        }
    }

    /// Parse a `definition.json` document (`{"colors": {"key": "#rrggbb", ...}}`).
    #[must_use]
    pub fn from_json(src: &str) -> Option<Self> {
        #[derive(Deserialize)]
        struct ThemeDoc {
            colors: Option<HashMap<String, String>>,
        }

        let doc: ThemeDoc = serde_json::from_str(src).ok()?;
        let raw_colors = doc.colors?;
        if raw_colors.is_empty() {
            return None;
        }

        let mut colors = HashMap::with_capacity(raw_colors.len());
        for (key, val) in raw_colors {
            if let Some(color) = hex(&val) {
                colors.insert(key, color);
            }
        }
        if colors.is_empty() {
            return None;
        }
        Some(Self { colors })
    }

    /// Parse a GTK/Waybar `theme.css` (`@define-color <name> <#rrggbb>;`).
    ///
    /// Scans lines in a single linear pass into a color map.
    #[must_use]
    pub fn from_css(src: &str) -> Option<Self> {
        let mut colors = HashMap::with_capacity(16);
        for line in src.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("@define-color ") {
                if let Some((name, val)) = rest.trim_end_matches(';').trim().split_once(' ') {
                    if let Some(color) = hex(val.trim()) {
                        colors.insert(name.trim().to_string(), color);
                    }
                }
            }
        }
        if colors.is_empty() {
            return None;
        }
        Some(Self { colors })
    }

    /// Look up a raw color token by name.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Color> {
        self.colors.get(key).copied()
    }

    /// Find the first matching color from a priority list of candidate keys.
    #[must_use]
    pub fn find(&self, candidates: &[&str]) -> Option<Color> {
        candidates.iter().find_map(|&key| self.get(key))
    }

    /// Map the raw color tokens into a Material 3 [`ColorScheme`].
    #[must_use]
    pub fn color_scheme(&self) -> Option<ColorScheme> {
        let primary = self.find(&[
            "primary", "accent", "mauve", "blue", "color4", "color5", "cursor", "color12",
        ])?;
        let surface = self.find(&["surface", "base", "background", "bg", "color0"])?;
        let on_surface = self.find(&["on_surface", "text", "foreground", "fg", "color15"])?;

        let on_primary = self
            .find(&["on_primary", "base", "background", "bg", "crust", "color0"])
            .unwrap_or(surface);
        let secondary = self
            .find(&["secondary", "green", "active", "color2", "color10", "teal"])
            .unwrap_or(primary);
        let on_secondary = self
            .find(&["on_secondary", "on_primary"])
            .unwrap_or(on_primary);
        let tertiary = self
            .find(&[
                "tertiary", "gauge", "blue", "cyan", "sky", "color6", "color14", "color4",
                "sapphire",
            ])
            .unwrap_or(primary);
        let on_tertiary = self
            .find(&["on_tertiary", "on_primary"])
            .unwrap_or(on_primary);
        let surface_container = self
            .find(&[
                "surface_container",
                "surface0",
                "card",
                "surface1",
                "mantle",
            ])
            .unwrap_or(surface);
        let on_surface_variant = self
            .find(&[
                "on_surface_variant",
                "muted",
                "overlay1",
                "subtext0",
                "subtext1",
                "color7",
            ])
            .unwrap_or(on_surface);
        let outline = self
            .find(&[
                "outline", "border", "overlay0", "overlay1", "surface1", "color8",
            ])
            .unwrap_or(on_surface_variant);
        let outline_variant = self
            .find(&[
                "outline_variant",
                "gauge_empty",
                "trough",
                "surface1",
                "overlay0",
                "color8",
                "color0",
            ])
            .unwrap_or(outline);
        let error = self
            .find(&[
                "error", "danger", "overload", "red", "color1", "color9", "maroon",
            ])
            .unwrap_or(primary);
        let on_error = self.find(&["on_error", "on_primary"]).unwrap_or(on_primary);

        Some(ColorScheme {
            primary,
            on_primary,
            secondary,
            on_secondary,
            tertiary,
            on_tertiary,
            surface,
            surface_container,
            on_surface,
            on_surface_variant,
            outline,
            outline_variant,
            error,
            on_error,
        })
    }

    /// The engine theme derived via Material 3 [`ColorScheme`].
    #[must_use]
    pub fn theme(&self) -> Theme {
        self.color_scheme()
            .map_or_else(|| Theme::DEFAULT, |scheme| scheme.to_theme())
    }
}

/// The palette of the active theme, resolved once per process.
///
/// `None` when no theme file is readable — callers then keep the built-in
/// ANSI theme.
#[must_use]
pub fn active() -> Option<&'static Palette> {
    static ACTIVE: OnceLock<Option<Palette>> = OnceLock::new();
    ACTIVE.get_or_init(resolve).as_ref()
}

/// Resolve the palette from the environment.
fn resolve() -> Option<Palette> {
    if let Some(path) = std::env::var_os("FLEX_THEME_FILE") {
        return Palette::from_path(Path::new(&path));
    }
    let dir = theme_dir()?;
    Palette::from_path(&dir.join("definition.json"))
        .or_else(|| Palette::from_path(&dir.join("theme.css")))
}

/// `~/.config/themes/current`, the directory `flex-theme activate` fills.
fn theme_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("themes")
            .join("current"),
    )
}

/// Parse `#rrggbb` securely without panics on malformed or non-ASCII input.
fn hex(value: &str) -> Option<Color> {
    let digits = value.strip_prefix('#')?;
    if digits.len() != 6 || !digits.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&digits[0..2], 16).ok()?;
    let g = u8::from_str_radix(&digits[2..4], 16).ok()?;
    let b = u8::from_str_radix(&digits[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    const NEON: &str = r##"{
        "name": "neon",
        "colors": {
            "base": "#0a0a14", "text": "#e6e6ff",
            "mauve": "#c04cff", "blue": "#5c7cff",
            "green": "#2bff88", "red": "#ff2e6e",
            "surface0": "#14141f", "surface1": "#1e1e30",
            "overlay0": "#3d3d5c", "overlay1": "#55557a"
        }
    }"##;

    #[test]
    fn parses_definition_json() {
        let p = Palette::from_json(NEON).expect("valid definition");
        assert_eq!(p.get("base"), Some(Color::Rgb(10, 10, 20)));
        assert_eq!(p.get("mauve"), Some(Color::Rgb(192, 76, 255)));
        assert_eq!(p.get("overlay1"), Some(Color::Rgb(85, 85, 122)));
    }

    #[test]
    fn parses_theme_css() {
        let css = "/* generated */\n@define-color base #0a0a14;\n\
                   @define-color text #e6e6ff;\n@define-color mauve #c04cff;\n\
                   @define-color blue #5c7cff;\n@define-color green #2bff88;\n\
                   @define-color red #ff2e6e;\n@define-color surface0 #14141f;\n\
                   @define-color surface1 #1e1e30;\n@define-color overlay0 #3d3d5c;\n\
                   @define-color overlay1 #55557a;\n";
        let p = Palette::from_css(css).expect("valid css");
        assert_eq!(p, Palette::from_json(NEON).expect("valid json"));
    }

    #[test]
    fn empty_palette_is_rejected() {
        assert!(Palette::from_json(r#"{"colors": {}}"#).is_none());
        assert!(Palette::from_json("not json").is_none());
        assert!(Palette::from_css("/* empty */").is_none());
    }

    #[test]
    fn malformed_hex_is_rejected() {
        assert!(hex("#0a0a1").is_none());
        assert!(hex("0a0a14").is_none());
        assert!(hex("#gggggg").is_none());
        assert!(hex("#üüüüüü").is_none());
        assert_eq!(hex("#0a0a14"), Some(Color::Rgb(10, 10, 20)));
    }

    #[test]
    fn material_color_scheme_resolves_roles() {
        let p = Palette::from_json(NEON).expect("valid definition");
        let scheme = p.color_scheme().expect("derives color scheme");
        assert_eq!(scheme.primary, Color::Rgb(192, 76, 255));
        assert_eq!(scheme.surface, Color::Rgb(10, 10, 20));
        assert_eq!(scheme.on_primary, Color::Rgb(10, 10, 20));
        assert_eq!(scheme.tertiary, Color::Rgb(92, 124, 255));
        assert_eq!(scheme.error, Color::Rgb(255, 46, 110));
    }

    #[test]
    fn theme_maps_accent_gauge_and_chrome() {
        let p = Palette::from_json(NEON).expect("valid definition");
        let t = p.theme();
        let scheme = p.color_scheme().expect("scheme");
        assert_eq!(t.selector.fg, Some(scheme.primary));
        assert_eq!(t.tab_selected.fg, Some(scheme.primary));
        assert_eq!(t.tab_marker.fg, Some(scheme.primary));
        assert_eq!(t.filter_prompt.fg, Some(scheme.primary));
        assert_eq!(t.gauge_fill.fg, Some(scheme.tertiary));
        assert_eq!(t.gauge_empty.fg, Some(scheme.outline_variant));
        assert_eq!(t.meter_active.fg, Some(scheme.secondary));
        assert_eq!(t.meter_overload.fg, Some(scheme.error));
        assert_eq!(t.dropdown_border.fg, Some(scheme.outline));
        assert_eq!(t.help_border.fg, Some(scheme.outline));
        assert_eq!(t.hint.fg, Some(scheme.on_surface_variant));
        assert_eq!(t.offline.fg, Some(scheme.on_surface_variant));
        assert_eq!(t.dropdown_selected.fg, Some(scheme.on_primary));
        assert_eq!(t.dropdown_selected.bg, Some(scheme.primary));
    }

    #[test]
    fn base16_and_ansi_tokens_resolve_color_scheme() {
        let base16 = r##"{
            "colors": {
                "background": "#1a1b26",
                "foreground": "#a9b1d6",
                "cursor": "#c0caf5",
                "color0": "#1a1b26",
                "color1": "#f7768e",
                "color2": "#9ece6a",
                "color3": "#e0af68",
                "color4": "#7aa2f7",
                "color5": "#bb9af7",
                "color6": "#7dcfff",
                "color7": "#a9b1d6",
                "color8": "#414868",
                "color9": "#f7768e",
                "color10": "#9ece6a",
                "color11": "#e0af68",
                "color12": "#7aa2f7",
                "color13": "#bb9af7",
                "color14": "#7dcfff",
                "color15": "#c0caf5",
                "surface0": "#24283b",
                "surface1": "#2f3346",
                "overlay0": "#565f89"
            }
        }"##;
        let p = Palette::from_json(base16).expect("valid base16 definition");
        let scheme = p.color_scheme().expect("derives color scheme from base16");
        assert_eq!(scheme.primary, Color::Rgb(122, 162, 247)); // color4 blue/accent
        assert_eq!(scheme.secondary, Color::Rgb(158, 206, 106)); // color2 green
        assert_eq!(scheme.tertiary, Color::Rgb(125, 207, 255)); // color6 cyan/sky
        assert_eq!(scheme.error, Color::Rgb(247, 118, 142)); // color1 red
        assert_eq!(scheme.surface, Color::Rgb(26, 27, 38)); // background
        assert_eq!(scheme.surface_container, Color::Rgb(36, 40, 59)); // surface0
        assert_eq!(scheme.outline, Color::Rgb(86, 95, 137)); // overlay0
    }
}
