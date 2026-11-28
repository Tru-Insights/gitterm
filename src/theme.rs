#![allow(dead_code)]
use iced::{color, Color, Theme};

// App theme (affects entire UI)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum AppTheme {
    #[default]
    Dark,
    Light,
}

impl AppTheme {
    pub fn toggle(&self) -> Self {
        match self {
            AppTheme::Dark => AppTheme::Light,
            AppTheme::Light => AppTheme::Dark,
        }
    }
}

// Theme color functions - complete Catppuccin palette for future use
// All color functions suppressed to avoid unused warnings
#[allow(dead_code)]
pub fn catppuccin_mocha_bg() -> Color {
    color!(0x1e1e2e)
}

pub fn catppuccin_mocha_surface0() -> Color {
    color!(0x313244)
}

pub fn catppuccin_mocha_surface1() -> Color {
    color!(0x45475a)
}

pub fn catppuccin_mocha_surface2() -> Color {
    color!(0x585b70)
}

pub fn catppuccin_mocha_text() -> Color {
    color!(0xcdd6f4)
}

pub fn catppuccin_mocha_subtext1() -> Color {
    color!(0xbac2de)
}

pub fn catppuccin_mocha_subtext0() -> Color {
    color!(0xa6adc8)
}

pub fn catppuccin_mocha_overlay2() -> Color {
    color!(0x9399b2)
}

pub fn catppuccin_mocha_overlay1() -> Color {
    color!(0x7f849c)
}

pub fn catppuccin_mocha_overlay0() -> Color {
    color!(0x6c7086)
}

pub fn catppuccin_mocha_blue() -> Color {
    color!(0x89b4fa)
}

pub fn catppuccin_mocha_lavender() -> Color {
    color!(0xb4befe)
}

pub fn catppuccin_mocha_sapphire() -> Color {
    color!(0x74c7ec)
}

pub fn catppuccin_mocha_sky() -> Color {
    color!(0x89dceb)
}

pub fn catppuccin_mocha_teal() -> Color {
    color!(0x94e2d5)
}

pub fn catppuccin_mocha_green() -> Color {
    color!(0xa6e3a1)
}

pub fn catppuccin_mocha_yellow() -> Color {
    color!(0xf9e2af)
}

pub fn catppuccin_mocha_peach() -> Color {
    color!(0xfab387)
}

pub fn catppuccin_mocha_maroon() -> Color {
    color!(0xeba0ac)
}

pub fn catppuccin_mocha_red() -> Color {
    color!(0xf38ba8)
}

pub fn catppuccin_mocha_mauve() -> Color {
    color!(0xcba6f7)
}

pub fn catppuccin_mocha_pink() -> Color {
    color!(0xf5c2e7)
}

pub fn catppuccin_mocha_flamingo() -> Color {
    color!(0xf2cdcd)
}

pub fn catppuccin_mocha_rosewater() -> Color {
    color!(0xf5e0dc)
}

// Latte colors (light theme)
pub fn catppuccin_latte_base() -> Color {
    color!(0xeff1f5)
}

pub fn catppuccin_latte_mantle() -> Color {
    color!(0xe6e9ef)
}

pub fn catppuccin_latte_crust() -> Color {
    color!(0xdce0e8)
}

pub fn catppuccin_latte_surface0() -> Color {
    color!(0xccd0da)
}

pub fn catppuccin_latte_surface1() -> Color {
    color!(0xbcc0cc)
}

pub fn catppuccin_latte_surface2() -> Color {
    color!(0xacb0be)
}

pub fn catppuccin_latte_text() -> Color {
    color!(0x4c4f69)
}

pub fn catppuccin_latte_subtext1() -> Color {
    color!(0x5c5f77)
}

pub fn catppuccin_latte_subtext0() -> Color {
    color!(0x6c6f85)
}

pub fn catppuccin_latte_overlay2() -> Color {
    color!(0x7c7f93)
}

pub fn catppuccin_latte_overlay1() -> Color {
    color!(0x8c8fa1)
}

pub fn catppuccin_latte_overlay0() -> Color {
    color!(0x9ca0b0)
}

pub fn catppuccin_latte_blue() -> Color {
    color!(0x1e66f5)
}

pub fn catppuccin_latte_lavender() -> Color {
    color!(0x7287fd)
}

pub fn catppuccin_latte_sapphire() -> Color {
    color!(0x209fb5)
}

pub fn catppuccin_latte_sky() -> Color {
    color!(0x04a5e5)
}

pub fn catppuccin_latte_teal() -> Color {
    color!(0x179299)
}

pub fn catppuccin_latte_green() -> Color {
    color!(0x40a02b)
}

pub fn catppuccin_latte_yellow() -> Color {
    color!(0xdf8e1d)
}

pub fn catppuccin_latte_peach() -> Color {
    color!(0xfe640b)
}

pub fn catppuccin_latte_maroon() -> Color {
    color!(0xe64553)
}

pub fn catppuccin_latte_red() -> Color {
    color!(0xd20f39)
}

pub fn catppuccin_latte_mauve() -> Color {
    color!(0x8839ef)
}

pub fn catppuccin_latte_pink() -> Color {
    color!(0xea76cb)
}

pub fn catppuccin_latte_flamingo() -> Color {
    color!(0xdd7878)
}

pub fn catppuccin_latte_rosewater() -> Color {
    color!(0xdc8a78)
}

impl AppTheme {
    // Background colors
    #[allow(dead_code)]
    pub fn bg(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_bg(),
            AppTheme::Light => catppuccin_latte_base(),
        }
    }

    pub fn surface0(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_surface0(),
            AppTheme::Light => catppuccin_latte_surface0(),
        }
    }

    pub fn surface1(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_surface1(),
            AppTheme::Light => catppuccin_latte_surface1(),
        }
    }

    pub fn surface2(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_surface2(),
            AppTheme::Light => catppuccin_latte_surface2(),
        }
    }

    // Text colors
    #[allow(dead_code)]
    pub fn text(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_text(),
            AppTheme::Light => catppuccin_latte_text(),
        }
    }

    pub fn subtext1(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_subtext1(),
            AppTheme::Light => catppuccin_latte_subtext1(),
        }
    }

    pub fn subtext0(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_subtext0(),
            AppTheme::Light => catppuccin_latte_subtext0(),
        }
    }

    // Accent colors
    pub fn blue(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_blue(),
            AppTheme::Light => catppuccin_latte_blue(),
        }
    }

    pub fn green(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_green(),
            AppTheme::Light => catppuccin_latte_green(),
        }
    }

    #[allow(dead_code)]
    pub fn red(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_red(),
            AppTheme::Light => catppuccin_latte_red(),
        }
    }

    pub fn yellow(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_yellow(),
            AppTheme::Light => catppuccin_latte_yellow(),
        }
    }

    pub fn mauve(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_mauve(),
            AppTheme::Light => catppuccin_latte_mauve(),
        }
    }

    #[allow(dead_code)]
    pub fn pink(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_pink(),
            AppTheme::Light => catppuccin_latte_pink(),
        }
    }

    pub fn peach(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_peach(),
            AppTheme::Light => catppuccin_latte_peach(),
        }
    }

    pub fn overlay0(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_overlay0(),
            AppTheme::Light => catppuccin_latte_overlay0(),
        }
    }

    pub fn overlay1(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_overlay1(),
            AppTheme::Light => catppuccin_latte_overlay1(),
        }
    }

    #[allow(dead_code)]
    pub fn overlay2(&self) -> Color {
        match self {
            AppTheme::Dark => catppuccin_mocha_overlay2(),
            AppTheme::Light => catppuccin_latte_overlay2(),
        }
    }

    // Convert to Iced theme
    #[allow(dead_code)]
    pub fn iced_theme(&self) -> Theme {
        match self {
            AppTheme::Dark => Theme::Dark,
            AppTheme::Light => Theme::Light,
        }
    }

    // Terminal color palette
    pub fn terminal_palette(&self) -> iced_term::ColorPalette {
        let palette = gitterm_core::terminal::palette(matches!(self, AppTheme::Dark));
        let hex = |rgb: u32| format!("#{rgb:06x}");
        iced_term::ColorPalette {
            background: hex(palette.background),
            foreground: hex(palette.foreground),
            black: hex(palette.ansi[0]),
            red: hex(palette.ansi[1]),
            green: hex(palette.ansi[2]),
            yellow: hex(palette.ansi[3]),
            blue: hex(palette.ansi[4]),
            magenta: hex(palette.ansi[5]),
            cyan: hex(palette.ansi[6]),
            white: hex(palette.ansi[7]),
            bright_black: hex(palette.ansi[8]),
            bright_red: hex(palette.ansi[9]),
            bright_green: hex(palette.ansi[10]),
            bright_yellow: hex(palette.ansi[11]),
            bright_blue: hex(palette.ansi[12]),
            bright_magenta: hex(palette.ansi[13]),
            bright_cyan: hex(palette.ansi[14]),
            bright_white: hex(palette.ansi[15]),
            bright_foreground: Some(hex(palette.foreground)),
            dim_foreground: hex(palette.dim_foreground),
            dim_black: hex(palette.dim[0]),
            dim_red: hex(palette.dim[1]),
            dim_green: hex(palette.dim[2]),
            dim_yellow: hex(palette.dim[3]),
            dim_blue: hex(palette.dim[4]),
            dim_magenta: hex(palette.dim[5]),
            dim_cyan: hex(palette.dim[6]),
            dim_white: hex(palette.dim[7]),
        }
    }

    // UI Colors
    pub fn bg_base(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x1e1e2e),
            AppTheme::Light => color!(0xeff1f5),
        }
    }

    pub fn bg_surface(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x181825),
            AppTheme::Light => color!(0xe6e9ef),
        }
    }

    pub fn bg_overlay(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x313244),
            AppTheme::Light => color!(0xdce0e8),
        }
    }

    pub fn text_primary(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0xcdd6f4),
            AppTheme::Light => color!(0x4c4f69),
        }
    }

    // Secondary/muted must stay on the palette's *text* tiers (subtext0,
    // overlay2). The old values reached down into overlay0/surface1, which
    // Catppuccin reserves for borders — as text they were unreadable on the
    // dark base/crust backgrounds.
    pub fn text_secondary(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0xa6adc8),
            AppTheme::Light => color!(0x6c6f85),
        }
    }

    pub fn text_muted(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x9399b2),
            AppTheme::Light => color!(0x7c7f93),
        }
    }

    pub fn accent(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x89b4fa),
            AppTheme::Light => color!(0x1e66f5),
        }
    }

    pub fn border(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x45475a),
            AppTheme::Light => color!(0xccd0da),
        }
    }

    pub fn success(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0xa6e3a1),
            AppTheme::Light => color!(0x40a02b),
        }
    }

    pub fn warning(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0xf9e2af),
            AppTheme::Light => color!(0xdf8e1d),
        }
    }

    pub fn danger(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0xf38ba8),
            AppTheme::Light => color!(0xd20f39),
        }
    }

    pub fn diff_add_bg(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x1a3a1a),
            AppTheme::Light => color!(0xd4f4d4),
        }
    }

    pub fn diff_del_bg(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x3a1a1a),
            AppTheme::Light => color!(0xf4d4d4),
        }
    }

    pub fn diff_add_highlight(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x3a6b3a),
            AppTheme::Light => color!(0x90d090),
        }
    }

    pub fn diff_del_highlight(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x6b3a3a),
            AppTheme::Light => color!(0xd09090),
        }
    }

    pub fn bg_crust(&self) -> Color {
        match self {
            AppTheme::Dark => color!(0x11111b),
            AppTheme::Light => color!(0xdce0e8),
        }
    }
}
