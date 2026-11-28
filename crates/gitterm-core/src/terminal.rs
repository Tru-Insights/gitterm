//! Terminal colors shared by the Iced and GPUI frontends. Values are RGB24.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalPalette {
    pub background: u32,
    pub foreground: u32,
    pub ansi: [u32; 16],
    pub dim_foreground: u32,
    pub dim: [u32; 8],
}

/// Iced AppTheme's Catppuccin Mocha / Latte terminal palettes.
pub fn palette(dark: bool) -> TerminalPalette {
    if dark {
        TerminalPalette {
            background: 0x1e1e2e,
            foreground: 0xcdd6f4,
            ansi: [
                0x45475a, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xbac2de,
                0x585b70, 0xf38ba8, 0xa6e3a1, 0xf9e2af, 0x89b4fa, 0xf5c2e7, 0x94e2d5, 0xa6adc8,
            ],
            dim_foreground: 0x7f849c,
            dim: [
                0x313244, 0xa65d6d, 0x6e9a6d, 0xa69a74, 0x5d78a6, 0xa6849c, 0x649a92, 0x7f849c,
            ],
        }
    } else {
        TerminalPalette {
            background: 0xeff1f5,
            foreground: 0x4c4f69,
            ansi: [
                0x5c5f77, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xacb0be,
                0x6c6f85, 0xd20f39, 0x40a02b, 0xdf8e1d, 0x1e66f5, 0xea76cb, 0x179299, 0xbcc0cc,
            ],
            dim_foreground: 0x6c6f85,
            dim: [
                0x4c4f69, 0xa10c2d, 0x338022, 0xb27117, 0x1852c4, 0xbb5ea2, 0x12747a, 0x8c8fa1,
            ],
        }
    }
}

/// Schemes detected by iced_term's terminal URL regex. OSC8 labels in GPUI
/// must not bypass that policy just because their destination is metadata.
pub fn link_scheme_allowed(url: &str) -> bool {
    [
        "ipfs:",
        "ipns:",
        "magnet:",
        "mailto:",
        "gemini://",
        "gopher://",
        "https://",
        "http://",
        "news:",
        "file://",
        "git://",
        "ssh:",
        "ftp://",
    ]
    .iter()
    .any(|scheme| {
        url.strip_prefix(scheme)
            .is_some_and(|rest| !rest.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_link_schemes_match_iced() {
        for url in [
            "ipfs:fixture",
            "ipns:fixture",
            "magnet:fixture",
            "mailto:owner@example.com",
            "gemini://example.com",
            "gopher://example.com",
            "https://example.com",
            "http://example.com",
            "news:fixture",
            "file:///tmp/fixture",
            "git://example.com/owner/repo",
            "ssh:example.com",
            "ftp://example.com",
        ] {
            assert!(link_scheme_allowed(url), "{url}");
        }
        for url in [
            "javascript:alert(1)",
            "data:text/html,fixture",
            "custom:fixture",
            "https://",
            "",
        ] {
            assert!(!link_scheme_allowed(url), "{url}");
        }
    }
}
