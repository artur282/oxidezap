use gpui::{Rgba, WindowAppearance, rgb};

use crate::settings::ThemeMode;

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
}

impl Theme {
    pub fn resolve(mode: ThemeMode, appearance: WindowAppearance) -> Self {
        let dark = match mode {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::System => matches!(
                appearance,
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
        };
        if dark { Self::dark() } else { Self::light() }
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_theme_follows_the_window_appearance() {
        let dark = Theme::resolve(ThemeMode::System, WindowAppearance::Dark);
        let light = Theme::resolve(ThemeMode::System, WindowAppearance::Light);
        assert_eq!(dark.bg, Theme::dark().bg);
        assert_eq!(light.bg, Theme::light().bg);
    }

    #[test]
    fn explicit_theme_wins() {
        let dark = Theme::resolve(ThemeMode::Dark, WindowAppearance::Light);
        assert_eq!(dark.bg, Theme::dark().bg);
        let light = Theme::resolve(ThemeMode::Light, WindowAppearance::Dark);
        assert_eq!(light.bg, Theme::light().bg);
    }
}
