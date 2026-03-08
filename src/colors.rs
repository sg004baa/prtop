use ratatui::style::Color;

#[derive(Debug, Clone)]
pub struct ColorScheme {
    pub app_title: Color,
    pub col_header: Color,
    pub role: Color,
    pub number: Color,
    pub repo: Color,
    pub new_pr: Color,
    pub draft: Color,
    pub footer_count: Color,
}

impl Default for ColorScheme {
    fn default() -> Self {
        Self {
            app_title: Color::Cyan,
            col_header: Color::DarkGray,
            role: Color::Cyan,
            number: Color::Yellow,
            repo: Color::Blue,
            new_pr: Color::Green,
            draft: Color::DarkGray,
            footer_count: Color::Green,
        }
    }
}

/// Parse a color string. Accepts named colors (case-insensitive) and `#rrggbb` hex.
pub fn parse_color(s: &str) -> Option<Color> {
    match s.to_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "dark_gray" | "darkgrey" | "dark_grey" => Some(Color::DarkGray),
        "lightred" | "light_red" => Some(Color::LightRed),
        "lightgreen" | "light_green" => Some(Color::LightGreen),
        "lightyellow" | "light_yellow" => Some(Color::LightYellow),
        "lightblue" | "light_blue" => Some(Color::LightBlue),
        "lightmagenta" | "light_magenta" => Some(Color::LightMagenta),
        "lightcyan" | "light_cyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        "reset" => Some(Color::Reset),
        s if s.starts_with('#') && s.len() == 7 => {
            let r = u8::from_str_radix(&s[1..3], 16).ok()?;
            let g = u8::from_str_radix(&s[3..5], 16).ok()?;
            let b = u8::from_str_radix(&s[5..7], 16).ok()?;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_colors_case_insensitive() {
        assert_eq!(parse_color("red"), Some(Color::Red));
        assert_eq!(parse_color("RED"), Some(Color::Red));
        assert_eq!(parse_color("Cyan"), Some(Color::Cyan));
        assert_eq!(parse_color("white"), Some(Color::White));
        assert_eq!(parse_color("reset"), Some(Color::Reset));
    }

    #[test]
    fn gray_aliases() {
        assert_eq!(parse_color("gray"), Some(Color::Gray));
        assert_eq!(parse_color("grey"), Some(Color::Gray));
        assert_eq!(parse_color("darkgray"), Some(Color::DarkGray));
        assert_eq!(parse_color("dark_gray"), Some(Color::DarkGray));
        assert_eq!(parse_color("darkgrey"), Some(Color::DarkGray));
        assert_eq!(parse_color("dark_grey"), Some(Color::DarkGray));
    }

    #[test]
    fn light_variants() {
        assert_eq!(parse_color("lightred"), Some(Color::LightRed));
        assert_eq!(parse_color("light_red"), Some(Color::LightRed));
        assert_eq!(parse_color("lightgreen"), Some(Color::LightGreen));
        assert_eq!(parse_color("light_cyan"), Some(Color::LightCyan));
    }

    #[test]
    fn hex_rgb() {
        assert_eq!(parse_color("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("#00ff00"), Some(Color::Rgb(0, 255, 0)));
        assert_eq!(parse_color("#0000ff"), Some(Color::Rgb(0, 0, 255)));
        assert_eq!(parse_color("#1a2b3c"), Some(Color::Rgb(0x1a, 0x2b, 0x3c)));
        assert_eq!(parse_color("#000000"), Some(Color::Rgb(0, 0, 0)));
        assert_eq!(parse_color("#ffffff"), Some(Color::Rgb(255, 255, 255)));
    }

    #[test]
    fn invalid_returns_none() {
        assert_eq!(parse_color("notacolor"), None);
        assert_eq!(parse_color(""), None);
        assert_eq!(parse_color("#12345"), None);   // too short
        assert_eq!(parse_color("#1234567"), None); // too long
        assert_eq!(parse_color("#gggggg"), None);  // invalid hex chars
        assert_eq!(parse_color("#"), None);
    }
}
