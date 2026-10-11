//! Shared styles for transcript roles, controls, instructions and markdown.
//!
//! The default leaves ordinary text and its background to the terminal. Explicit light and dark
//! palettes paint their background too, so their contrast is a property of the pair rather than
//! a guess about the terminal. High contrast uses the terminal's own colours and no dimming.

use ratatui::style::{Color, Modifier, Style};

use crate::transcript::{Entry, EntryKind, Role};

/// Colours and emphasis for the whole interface, not only the transcript.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Theme {
    /// Human input and title accents.
    pub user: Style,
    /// Ordinary text and the screen background.
    pub assistant: Style,
    /// Model reasoning and code comments.
    pub reasoning: Style,
    /// Tool activity.
    pub tool: Style,
    /// Harness notices and links.
    pub notice: Style,
    /// Errors and denial actions.
    pub error: Style,
    /// An open turn's status.
    pub busy: Style,
    /// Instructions and secondary information; never dimmed.
    pub secondary: Style,
    /// An affirmative action.
    pub success: Style,
    /// The selected menu item.
    pub selected: Style,
    /// Rules, diagram edges and borders.
    pub border: Style,
}

impl Default for Theme {
    fn default() -> Self {
        let plain = Style::new();
        Self {
            user: plain.fg(Color::Cyan).add_modifier(Modifier::BOLD),
            assistant: plain,
            reasoning: plain.add_modifier(Modifier::ITALIC),
            tool: plain.fg(Color::Yellow),
            notice: plain.add_modifier(Modifier::BOLD),
            error: plain.fg(Color::Red).add_modifier(Modifier::BOLD),
            busy: plain.fg(Color::Magenta),
            secondary: plain,
            success: plain.fg(Color::Green).add_modifier(Modifier::BOLD),
            selected: plain.add_modifier(Modifier::BOLD | Modifier::REVERSED),
            border: plain,
        }
    }
}

impl Theme {
    /// Uses dark text and accents against an explicit white background.
    #[must_use]
    pub fn light() -> Self {
        Self::palette(
            Color::Rgb(255, 255, 255),
            [
                Color::Rgb(32, 32, 32),
                Color::Rgb(0, 83, 102),
                Color::Rgb(105, 66, 0),
                Color::Rgb(0, 61, 153),
                Color::Rgb(153, 0, 0),
                Color::Rgb(107, 0, 107),
                Color::Rgb(0, 92, 0),
            ],
        )
    }

    /// Uses light text and accents against an explicit near-black background.
    #[must_use]
    pub fn dark() -> Self {
        Self::palette(
            Color::Rgb(16, 16, 16),
            [
                Color::Rgb(238, 238, 238),
                Color::Rgb(102, 221, 238),
                Color::Rgb(255, 221, 102),
                Color::Rgb(153, 187, 255),
                Color::Rgb(255, 153, 153),
                Color::Rgb(238, 153, 238),
                Color::Rgb(153, 238, 153),
            ],
        )
    }

    /// Uses the terminal's own foreground and background, with no dimming or colour accents.
    #[must_use]
    pub fn monochrome() -> Self {
        Self::default().without_color()
    }

    /// Removes every colour request while preserving focus and emphasis modifiers.
    ///
    /// Suppression belongs here rather than in the backend: a suppressed colour command can reset
    /// the caret's reversal. This also honours `NO_COLOR` for an explicitly configured palette.
    #[must_use]
    pub fn without_color(mut self) -> Self {
        for style in [
            &mut self.user,
            &mut self.assistant,
            &mut self.reasoning,
            &mut self.tool,
            &mut self.notice,
            &mut self.error,
            &mut self.busy,
            &mut self.secondary,
            &mut self.success,
            &mut self.selected,
            &mut self.border,
        ] {
            style.fg = None;
            style.bg = None;
            style.underline_color = None;
        }
        self
    }

    /// Builds a complete palette, with ordinary text used for all explanatory information.
    fn palette(background: Color, colors: [Color; 7]) -> Self {
        let [text, user, tool, notice, error, busy, success] = colors;
        let plain = Style::new().fg(text).bg(background);
        Self {
            user: plain.fg(user).add_modifier(Modifier::BOLD),
            assistant: plain,
            reasoning: plain.add_modifier(Modifier::ITALIC),
            tool: plain.fg(tool),
            notice: plain.fg(notice),
            error: plain.fg(error).add_modifier(Modifier::BOLD),
            busy: plain.fg(busy),
            secondary: plain,
            success: plain.fg(success).add_modifier(Modifier::BOLD),
            selected: plain.add_modifier(Modifier::BOLD | Modifier::REVERSED),
            border: plain,
        }
    }

    /// The style for a transcript role.
    #[must_use]
    pub const fn style_for(&self, role: Role) -> Style {
        match role {
            Role::User => self.user,
            Role::Assistant => self.assistant,
            Role::Reasoning => self.reasoning,
            Role::Tool => self.tool,
            Role::Harness => self.notice,
        }
    }

    /// The style for an entry, accounting for tool failure.
    #[must_use]
    pub const fn style_for_entry(&self, entry: &Entry) -> Style {
        if let EntryKind::ToolResult { is_error: true, .. } = entry.kind() {
            return self.error;
        }
        self.style_for(entry.role())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color) -> f64 {
        let Color::Rgb(r, g, b) = color else {
            panic!("explicit RGB palette");
        };
        let linear = |channel: u8| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.0722_f64.mul_add(linear(b), 0.7152_f64.mul_add(linear(g), 0.2126 * linear(r)))
    }

    #[test]
    fn explicit_palettes_meet_text_contrast_for_every_role_and_control() {
        for theme in [Theme::light(), Theme::dark()] {
            for style in [
                theme.user,
                theme.assistant,
                theme.reasoning,
                theme.tool,
                theme.notice,
                theme.error,
                theme.busy,
                theme.secondary,
                theme.success,
                theme.selected,
                theme.border,
            ] {
                let foreground = luminance(style.fg.expect("foreground"));
                let background = luminance(style.bg.expect("background"));
                let ratio =
                    (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05);
                assert!(ratio >= 4.5, "{style:?} contrast is {ratio:.2}:1");
                assert!(!style.add_modifier.contains(Modifier::DIM));
            }
        }
    }

    #[test]
    fn ordinary_and_monochrome_styles_inherit_terminal_colors() {
        assert_eq!(Theme::default().assistant, Style::new());
        assert_eq!(
            Theme::monochrome().selected.add_modifier,
            Modifier::BOLD | Modifier::REVERSED
        );
        for theme in [Theme::default(), Theme::light(), Theme::dark()] {
            let mono = theme.without_color();
            for style in [
                mono.user,
                mono.assistant,
                mono.reasoning,
                mono.tool,
                mono.notice,
                mono.error,
                mono.busy,
                mono.secondary,
                mono.success,
                mono.selected,
                mono.border,
            ] {
                assert!(
                    style.fg.is_none() && style.bg.is_none() && style.underline_color.is_none()
                );
                assert!(!style.add_modifier.contains(Modifier::DIM));
            }
        }
    }
}
