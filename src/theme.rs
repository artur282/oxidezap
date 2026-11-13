use gpui::{Pixels, Rgba, WindowAppearance, px, rgb};

use crate::settings::{AccessibilitySettings, ThemeMode};

/// GPUI's default em size. A window multiplies this by the selected font scale
/// so that rem-based text and line heights follow the accessibility setting.
const BASE_REM_SIZE: f32 = 16.0;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub bg: Rgba,
    pub surface: Rgba,
    pub surface_alt: Rgba,
    pub surface_hover: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub text_muted: Rgba,
    pub text_faint: Rgba,
    pub accent: Rgba,
    pub accent_hover: Rgba,
    pub accent_text: Rgba,
    pub danger: Rgba,
    /// Multiplier applied by [`Theme::font_px`] and [`Theme::rem_size`].
    font_scale: f32,
}

impl Theme {
    pub fn resolve(
        mode: ThemeMode,
        appearance: WindowAppearance,
        accessibility: &AccessibilitySettings,
    ) -> Self {
        let dark = match mode {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::System => matches!(
                appearance,
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
        };
        let mut theme = match (dark, accessibility.high_contrast) {
            (true, true) => Self::dark_high_contrast(),
            (true, false) => Self::dark(),
            (false, true) => Self::light_high_contrast(),
            (false, false) => Self::light(),
        };
        theme.font_scale = accessibility.font_scale.factor();
        theme
    }

    pub fn dark() -> Self {
        Self {
            bg: rgb(0x0b141a),
            surface: rgb(0x111b21),
            surface_alt: rgb(0x182229),
            surface_hover: rgb(0x202c33),
            border: rgb(0x222d34),
            text: rgb(0xe9edef),
            text_muted: rgb(0x8696a0),
            text_faint: rgb(0x667781),
            accent: rgb(0x00a884),
            accent_hover: rgb(0x06cf9c),
            accent_text: rgb(0x0b141a),
            danger: rgb(0xf15c65),
            font_scale: 1.0,
        }
    }

    pub fn light() -> Self {
        Self {
            bg: rgb(0xf0f2f5),
            surface: rgb(0xffffff),
            surface_alt: rgb(0xf7f8fa),
            surface_hover: rgb(0xe9edef),
            border: rgb(0xd1d7db),
            text: rgb(0x111b21),
            text_muted: rgb(0x667781),
            text_faint: rgb(0x8696a0),
            accent: rgb(0x008069),
            accent_hover: rgb(0x00a884),
            accent_text: rgb(0xffffff),
            danger: rgb(0xd93025),
            font_scale: 1.0,
        }
    }

    /// Black background, white text, and borders at or above the WCAG AAA
    /// non-text contrast ratio. The accent keeps a dark foreground so buttons
    /// stay readable.
    pub fn dark_high_contrast() -> Self {
        Self {
            bg: rgb(0x000000),
            surface: rgb(0x0d0d0d),
            surface_alt: rgb(0x1a1a1a),
            surface_hover: rgb(0x333333),
            border: rgb(0x8c8c8c),
            text: rgb(0xffffff),
            text_muted: rgb(0xd9d9d9),
            text_faint: rgb(0xb3b3b3),
            accent: rgb(0x00e08a),
            accent_hover: rgb(0x4dffb3),
            accent_text: rgb(0x000000),
            danger: rgb(0xff8a8a),
            font_scale: 1.0,
        }
    }

    /// White background, black text, and dark borders. The green accent is
    /// darkened enough for white button text to reach the same contrast class.
    pub fn light_high_contrast() -> Self {
        Self {
            bg: rgb(0xffffff),
            surface: rgb(0xffffff),
            surface_alt: rgb(0xf0f0f0),
            surface_hover: rgb(0xd9d9d9),
            border: rgb(0x595959),
            text: rgb(0x000000),
            text_muted: rgb(0x262626),
            text_faint: rgb(0x404040),
            accent: rgb(0x005c4b),
            accent_hover: rgb(0x007a63),
            accent_text: rgb(0xffffff),
            danger: rgb(0xa8001d),
            font_scale: 1.0,
        }
    }

    /// Window em size for the current font scale. Callers pass it to
    /// `Window::set_rem_size`; explicit typography uses [`Theme::font_px`].
    pub fn rem_size(&self) -> Pixels {
        px(BASE_REM_SIZE * self.font_scale)
    }

    /// Scales a base font size. Only typography is scaled; icons, spacing, and
    /// window geometry keep their fixed sizes.
    pub fn font_px(&self, base: f32) -> Pixels {
        px(base * self.font_scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_accessibility() -> AccessibilitySettings {
        AccessibilitySettings::default()
    }

    fn contrast(left: Rgba, right: Rgba) -> f32 {
        fn channel(value: f32) -> f32 {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        }
        let luminance = |color: Rgba| {
            0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
        };
        let (left, right) = (luminance(left), luminance(right));
        (left.max(right) + 0.05) / (left.min(right) + 0.05)
    }

    fn assert_high_contrast(theme: Theme) {
        assert!(
            contrast(theme.text, theme.bg) >= 7.0,
            "text/bg contrast was {}",
            contrast(theme.text, theme.bg)
        );
        assert!(
            contrast(theme.text, theme.surface) >= 7.0,
            "text/surface contrast was {}",
            contrast(theme.text, theme.surface)
        );
        assert!(
            contrast(theme.text_muted, theme.surface) >= 7.0,
            "muted text contrast was {}",
            contrast(theme.text_muted, theme.surface)
        );
        assert!(
            contrast(theme.text_faint, theme.surface) >= 4.5,
            "faint text contrast was {}",
            contrast(theme.text_faint, theme.surface)
        );
        assert!(
            contrast(theme.border, theme.surface) >= 3.0,
            "border contrast was {}",
            contrast(theme.border, theme.surface)
        );
        assert!(
            contrast(theme.accent_text, theme.accent) >= 4.5,
            "accent contrast was {}",
            contrast(theme.accent_text, theme.accent)
        );
    }

    #[test]
    fn system_theme_follows_the_window_appearance() {
        let accessibility = default_accessibility();
        let dark = Theme::resolve(ThemeMode::System, WindowAppearance::Dark, &accessibility);
        let light = Theme::resolve(ThemeMode::System, WindowAppearance::Light, &accessibility);
        assert_eq!(dark.bg, Theme::dark().bg);
        assert_eq!(light.bg, Theme::light().bg);
    }

    #[test]
    fn explicit_theme_wins() {
        let accessibility = default_accessibility();
        let dark = Theme::resolve(ThemeMode::Dark, WindowAppearance::Light, &accessibility);
        assert_eq!(dark.bg, Theme::dark().bg);
        let light = Theme::resolve(ThemeMode::Light, WindowAppearance::Dark, &accessibility);
        assert_eq!(light.bg, Theme::light().bg);
    }

    #[test]
    fn high_contrast_keeps_the_selected_base_mode() {
        let accessibility = AccessibilitySettings {
            high_contrast: true,
            ..AccessibilitySettings::default()
        };
        let dark = Theme::resolve(ThemeMode::Dark, WindowAppearance::Light, &accessibility);
        let light = Theme::resolve(ThemeMode::Light, WindowAppearance::Dark, &accessibility);
        let system_dark = Theme::resolve(ThemeMode::System, WindowAppearance::Dark, &accessibility);
        assert_eq!(dark.bg, Theme::dark_high_contrast().bg);
        assert_eq!(light.bg, Theme::light_high_contrast().bg);
        assert_eq!(system_dark.bg, Theme::dark_high_contrast().bg);
    }

    #[test]
    fn high_contrast_palettes_reach_the_documented_contrast_ratios() {
        assert_high_contrast(Theme::dark_high_contrast());
        assert_high_contrast(Theme::light_high_contrast());
    }

    #[test]
    fn font_scale_multiplies_native_typography() {
        let accessibility = AccessibilitySettings {
            font_scale: crate::settings::FontScale::from_percent(130.0),
            ..AccessibilitySettings::default()
        };
        let theme = Theme::resolve(ThemeMode::Dark, WindowAppearance::Dark, &accessibility);
        assert!((f32::from(theme.font_px(10.0)) - 13.0).abs() < 0.0001);
        assert!((f32::from(theme.rem_size()) - 20.8).abs() < 0.0001);
        assert!((f32::from(theme.font_px(100.0)) - 130.0).abs() < 0.0001);
    }
}
