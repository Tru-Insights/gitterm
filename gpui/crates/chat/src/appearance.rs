/// Host-owned chat settings, independent of the shell's UI/terminal typography.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChatAppearance {
    // Retained from the host setting; GPUI chat does not render Iced's HTML visual replies.
    pub light: bool,
    pub font_size: f32,
}

impl Default for ChatAppearance {
    fn default() -> Self {
        Self {
            light: false,
            font_size: 15.,
        }
    }
}

impl ChatAppearance {
    pub(crate) fn normalized(self) -> Self {
        assert!(self.font_size.is_finite(), "Invalid chat font size");
        Self {
            font_size: self.font_size.clamp(11., 22.),
            ..self
        }
    }

    /// CSS sizes are expressed relative to a 14px chat in agent_chat.html.
    pub(crate) fn size(self, css_size: f32) -> f32 {
        self.font_size * css_size / 14.
    }

    pub fn palette(self) -> ChatPalette {
        ChatPalette {
            base: 0x1e1e2e,
            alt: 0x181825,
            block: 0x313244,
            footer: 0x2a2b3d,
            fg: 0xbac2de,
            muted: 0x7f849c,
            accent: 0x89b4fa,
            border: 0x45475a,
            strong: 0x585b70,
            user: 0xa6e3a1,
            result: 0x94e2d5,
            tool: 0xfab387,
            warn: 0xf9e2af,
            err: 0xf38ba8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChatPalette {
    pub base: u32,
    pub alt: u32,
    pub block: u32,
    pub footer: u32,
    pub fg: u32,
    pub muted: u32,
    pub accent: u32,
    pub border: u32,
    pub strong: u32,
    pub user: u32,
    pub result: u32,
    pub tool: u32,
    pub warn: u32,
    pub err: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_matches_chat_css_and_stays_dark_with_light_appearance() {
        let dark = ChatAppearance::default().palette();
        let css = include_str!("../../../../assets/agent_chat.html");
        for (name, color) in [
            ("bg", dark.base),
            ("bg-alt", dark.alt),
            ("bg-block", dark.block),
            ("bg-footer", dark.footer),
            ("fg", dark.fg),
            ("fg-muted", dark.muted),
            ("accent", dark.accent),
            ("border", dark.border),
            ("border-strong", dark.strong),
            ("user", dark.user),
            ("result", dark.result),
            ("tool", dark.tool),
            ("warn", dark.warn),
            ("err", dark.err),
        ] {
            assert!(css.contains(&format!("--{name}: #{color:06x};")), "{name}");
        }
        let light = ChatAppearance {
            light: true,
            ..Default::default()
        }
        .palette();
        assert_eq!(light, dark);
    }

    #[test]
    #[should_panic(expected = "Invalid chat font size")]
    fn invalid_font_size_is_rejected() {
        ChatAppearance {
            font_size: f32::NAN,
            ..Default::default()
        }
        .normalized();
    }

    #[test]
    fn font_size_uses_iced_bounds_and_proportions() {
        for (input, expected) in [(9., 11.), (18., 18.), (40., 22.)] {
            let appearance = ChatAppearance {
                font_size: input,
                ..Default::default()
            }
            .normalized();
            assert_eq!(appearance.font_size, expected);
            assert_eq!(appearance.size(13.5), expected * 13.5 / 14.);
        }
    }
}
