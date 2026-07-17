//! TUI color theme.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub user_bar: Color,
    pub user_bg: Color,
    pub assistant_bar: Color,
    pub assistant_bg: Color,
    pub system_bar: Color,
    pub tool_bar: Color,
    pub question_bar: Color,
    pub status_bg: Color,
    pub status_fg: Color,
    pub input_border: Color,
    pub selected_fg: Color,
    pub code_bg: Color,
    pub code_inline_bg: Color,
    pub link_fg: Color,
    pub error_fg: Color,
    pub spinner_fg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_bar: Color::Cyan,
            user_bg: Color::Rgb(45, 45, 55),
            assistant_bar: Color::Green,
            assistant_bg: Color::Rgb(28, 34, 28),
            system_bar: Color::Yellow,
            tool_bar: Color::DarkGray,
            question_bar: Color::Magenta,
            status_bg: Color::Rgb(40, 40, 50),
            status_fg: Color::Gray,
            input_border: Color::Blue,
            selected_fg: Color::Black,
            code_bg: Color::Rgb(30, 30, 30),
            code_inline_bg: Color::Rgb(40, 40, 40),
            link_fg: Color::LightBlue,
            error_fg: Color::Red,
            spinner_fg: Color::Green,
        }
    }
}

impl Theme {
    pub fn default_dark() -> Self {
        Self::default()
    }

    pub fn default_light() -> Self {
        Self::default()
    }
}
