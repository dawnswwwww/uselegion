//! TUI color theme.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub user_bar: Color,
    pub user_bg: Color,
    pub assistant_bar: Color,
    pub assistant_bg: Color,
    pub system_bar: Color,
    pub system_bg: Color,
    pub tool_bar: Color,
    pub question_bar: Color,
    pub question_bg: Color,
    pub status_bg: Color,
    pub status_fg: Color,
    pub input_border: Color,
    pub selected_fg: Color,
    pub selected_bg: Color,
    pub code_bg: Color,
    pub code_fg: Color,
    pub code_gutter_fg: Color,
    pub code_inline_bg: Color,
    pub inline_code_fg: Color,
    pub link_fg: Color,
    pub error_fg: Color,
    pub spinner_fg: Color,
    /// Foreground colors for heading levels 1-6.
    pub headings: [Color; 6],
    /// syntect theme name used for code-block highlighting. Must exist in
    /// syntect's default `ThemeSet` (`syntax.rs` falls back to the dark
    /// theme when it does not).
    pub syntect_theme: &'static str,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_bar: Color::Cyan,
            user_bg: Color::Rgb(45, 45, 55),
            assistant_bar: Color::Green,
            assistant_bg: Color::Rgb(28, 34, 28),
            system_bar: Color::Yellow,
            system_bg: Color::Rgb(42, 40, 26),
            tool_bar: Color::DarkGray,
            question_bar: Color::Magenta,
            question_bg: Color::Rgb(48, 36, 48),
            status_bg: Color::Rgb(40, 40, 50),
            status_fg: Color::Gray,
            input_border: Color::Blue,
            selected_fg: Color::Black,
            selected_bg: Color::Blue,
            code_bg: Color::Rgb(30, 30, 30),
            code_fg: Color::Rgb(220, 220, 220),
            code_gutter_fg: Color::DarkGray,
            code_inline_bg: Color::Rgb(40, 40, 40),
            inline_code_fg: Color::White,
            link_fg: Color::LightBlue,
            error_fg: Color::Red,
            spinner_fg: Color::Green,
            headings: [
                Color::LightGreen,
                Color::Green,
                Color::Cyan,
                Color::Yellow,
                Color::Magenta,
                Color::DarkGray,
            ],
            syntect_theme: "base16-ocean.dark",
        }
    }
}

impl Theme {
    /// All theme names accepted by `by_name`, for help text and validation.
    pub const NAMES: &'static [&'static str] = &["default", "dark", "light"];

    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "default" | "dark" => Some(Self::default_dark()),
            "light" => Some(Self::default_light()),
            _ => None,
        }
    }

    pub fn default_dark() -> Self {
        Self::default()
    }

    pub fn default_light() -> Self {
        Self {
            user_bar: Color::Rgb(0, 100, 160),
            user_bg: Color::Rgb(225, 235, 245),
            assistant_bar: Color::Rgb(20, 120, 50),
            assistant_bg: Color::Rgb(232, 242, 232),
            system_bar: Color::Rgb(150, 110, 0),
            system_bg: Color::Rgb(245, 240, 220),
            tool_bar: Color::DarkGray,
            question_bar: Color::Rgb(140, 40, 140),
            question_bg: Color::Rgb(242, 230, 242),
            status_bg: Color::Rgb(220, 220, 228),
            status_fg: Color::Rgb(80, 80, 90),
            input_border: Color::Rgb(0, 100, 160),
            selected_fg: Color::White,
            selected_bg: Color::Rgb(0, 100, 160),
            code_bg: Color::Rgb(240, 240, 240),
            code_fg: Color::Rgb(40, 40, 40),
            code_gutter_fg: Color::Rgb(150, 150, 150),
            code_inline_bg: Color::Rgb(230, 230, 230),
            inline_code_fg: Color::Rgb(30, 30, 30),
            link_fg: Color::Rgb(0, 90, 180),
            error_fg: Color::Rgb(180, 30, 30),
            spinner_fg: Color::Rgb(20, 120, 50),
            headings: [
                Color::Rgb(20, 120, 50),
                Color::Rgb(20, 120, 50),
                Color::Rgb(0, 100, 140),
                Color::Rgb(150, 110, 0),
                Color::Rgb(140, 40, 140),
                Color::DarkGray,
            ],
            syntect_theme: "InspiredGitHub",
        }
    }

    /// Foreground color for a markdown heading of `level` (1-6); out-of-range
    /// levels clamp to the nearest entry.
    pub fn heading_color(&self, level: u8) -> Color {
        let idx = (level as usize)
            .saturating_sub(1)
            .min(self.headings.len() - 1);
        self.headings[idx]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_name_resolves_known_themes() {
        assert!(Theme::by_name("default").is_some());
        assert!(Theme::by_name("dark").is_some());
        assert!(Theme::by_name("light").is_some());
        assert!(Theme::by_name("solarized").is_none());
    }

    #[test]
    fn light_theme_differs_from_dark() {
        assert_ne!(Theme::default_light(), Theme::default_dark());
    }

    #[test]
    fn names_lists_all_resolvable_themes() {
        for name in Theme::NAMES {
            assert!(Theme::by_name(name).is_some(), "{name} must resolve");
        }
    }

    #[test]
    fn heading_color_is_clamped_for_all_levels() {
        let theme = Theme::default();
        // Levels 1..=6 map to the array; out-of-range levels clamp, not panic.
        let _ = theme.heading_color(1);
        let _ = theme.heading_color(6);
        let _ = theme.heading_color(0);
        let _ = theme.heading_color(255);
    }
}
