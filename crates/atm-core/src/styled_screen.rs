//! Strict, vendor-neutral decoding of tmux's styled live viewport.
//!
//! This is not a terminal emulator. Only the captured SGR subset and OSC-8 link
//! metadata are accepted; unknown controls invalidate the entire observation.
//! Offsets address UTF-8 character starts, never terminal cells. Spaces and LF
//! are retained exactly, including tmux's allocated grid padding.

#[derive(Debug, Clone, Copy, Default)]
struct Style {
    faint: bool,
    inverse: bool,
}

#[derive(Debug, Clone)]
pub struct StyledScreen {
    text: String,
    styles: Vec<(usize, Style)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unsupported or malformed captured terminal text")]
pub struct InvalidStyledScreen;

impl StyledScreen {
    pub fn parse(text: &str) -> Result<Self, InvalidStyledScreen> {
        let mut screen = Self {
            text: String::new(),
            styles: Vec::new(),
        };
        let mut style = Style::default();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.next() {
                    Some('[') => {
                        let mut parameters = String::new();
                        loop {
                            match chars.next() {
                                Some('m') => break,
                                Some(c) if c.is_ascii_digit() || c == ';' => parameters.push(c),
                                _ => return Err(InvalidStyledScreen),
                            }
                        }
                        style.apply(&parameters)?;
                    }
                    Some(']') => {
                        let mut metadata = String::new();
                        loop {
                            match chars.next() {
                                Some('\x07') => break,
                                Some('\x1b') if chars.next() == Some('\\') => break,
                                Some(c) if !c.is_control() => metadata.push(c),
                                _ => return Err(InvalidStyledScreen),
                            }
                        }
                        if metadata
                            .strip_prefix("8;")
                            .and_then(|s| s.split_once(';'))
                            .is_none()
                        {
                            return Err(InvalidStyledScreen);
                        }
                    }
                    _ => return Err(InvalidStyledScreen),
                },
                '\n' => screen.text.push(c),
                c if c.is_control() => return Err(InvalidStyledScreen),
                c => {
                    screen.styles.push((screen.text.len(), style));
                    screen.text.push(c);
                }
            }
        }
        Ok(screen)
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn faint_at(&self, byte_offset: usize) -> Option<bool> {
        self.style_at(byte_offset).map(|s| s.faint)
    }

    #[must_use]
    pub fn inverse_at(&self, byte_offset: usize) -> Option<bool> {
        self.style_at(byte_offset).map(|s| s.inverse)
    }

    fn style_at(&self, offset: usize) -> Option<Style> {
        self.styles
            .binary_search_by_key(&offset, |(i, _)| *i)
            .ok()
            .and_then(|i| self.styles.get(i))
            .map(|(_, style)| *style)
    }
}

impl Style {
    fn apply(&mut self, parameters: &str) -> Result<(), InvalidStyledScreen> {
        if parameters.is_empty() {
            *self = Self::default();
            return Ok(());
        }
        let values = parameters
            .split(';')
            .map(|s| s.parse::<u8>().map_err(|_| InvalidStyledScreen))
            .collect::<Result<Vec<_>, _>>()?;
        let mut values = values.into_iter();
        while let Some(value) = values.next() {
            match value {
                0 => *self = Self::default(),
                2 => self.faint = true,
                7 => self.inverse = true,
                22 => self.faint = false,
                27 => self.inverse = false,
                1 | 3 | 23 | 30..=37 | 39 | 40..=47 | 49 | 90..=97 | 100..=107 => {}
                38 | 48 => {
                    let count = match values.next() {
                        Some(5) => 1,
                        Some(2) => 3,
                        _ => return Err(InvalidStyledScreen),
                    };
                    for _ in 0..count {
                        values.next().ok_or(InvalidStyledScreen)?;
                    }
                }
                _ => return Err(InvalidStyledScreen),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn sgr_state_and_resets() {
        let s = StyledScreen::parse("\x1b[1;2;3;7mA\x1b[22mB\x1b[23;27mC\x1b[0;2mD\x1b[mE  \n")
            .unwrap();
        assert_eq!(s.text(), "ABCDE  \n");
        for (offset, faint, inverse) in [
            (0, true, true),
            (1, false, true),
            (2, false, false),
            (3, true, false),
            (4, false, false),
        ] {
            assert_eq!(s.faint_at(offset), Some(faint));
            assert_eq!(s.inverse_at(offset), Some(inverse));
        }
    }

    #[test]
    fn color_parameters_are_not_attributes() {
        let s = StyledScreen::parse("\x1b[38;2;2;7;0;48;5;7mA\x1b[30;47;90;107;39;49mB").unwrap();
        assert_eq!(s.text(), "AB");
        for i in 0..2 {
            assert_eq!(s.faint_at(i), Some(false));
            assert_eq!(s.inverse_at(i), Some(false));
        }
    }

    #[test]
    fn styles_survive_line_breaks() {
        let s = StyledScreen::parse("\x1b[2mA \n B\x1b[0m").unwrap();
        assert_eq!(s.text(), "A \n B");
        assert_eq!(s.faint_at(4), Some(true));
        assert_eq!(s.faint_at(2), None);
    }

    #[test]
    fn osc8_preserves_label() {
        for end in ["\x07", "\x1b\\"] {
            let s = StyledScreen::parse(&format!(
                "\x1b]8;id=x;https://example.test{end}\x1b[2mlink\x1b]8;;{end}"
            ))
            .unwrap();
            assert_eq!(s.text(), "link");
            assert_eq!(s.faint_at(0), Some(true));
        }
    }

    #[test]
    fn malformed_or_unsupported_control_refuses() {
        for text in [
            "\x1b",
            "\x1b[",
            "\x1b[2",
            "\x1b[2;m",
            "\x1b[;2m",
            "\x1b[8m",
            "\x1b[28m",
            "\x1b[4m",
            "\x1b[2J",
            "\x1b[1A",
            "\x1b[38;5m",
            "\x1b[48;2;0;1m",
            "\x1b[38;5;256m",
            "\x1b[38;2;0;0;256m",
            "\x1b[38;3;1m",
            "\x1b[99999999999999999999m",
            "\x1b[2:7m",
            "\x1b]0;title\x07",
            "\x1b]8;;url",
            "\x1b]8;url\x07",
            "\x1b]8;;url\n\x07",
            "\t",
            "\r",
            "\x07",
            "\u{9b}",
            "\x7f",
        ] {
            assert!(StyledScreen::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn unicode_offsets_are_not_cursor_cells() {
        let s = StyledScreen::parse("\x1b[7m界é\n").unwrap();
        assert_eq!(s.inverse_at(0), Some(true));
        assert_eq!(s.inverse_at(3), Some(true));
        for i in [1, 2, 4, 5, 6, usize::MAX] {
            assert_eq!(s.inverse_at(i), None);
        }
    }

    proptest! {
        #[test]
        fn arbitrary_unicode_is_panic_free(text in proptest::collection::vec(any::<char>(), 0..1024)) {
            let text: String = text.into_iter().collect();
            if let Ok(screen) = StyledScreen::parse(&text) {
                prop_assert!(!screen.text().chars().any(|c| c.is_control() && c != '\n'));
                for (i,c) in screen.text().char_indices() {
                    prop_assert_eq!(screen.faint_at(i).is_some(), c != '\n');
                    prop_assert_eq!(screen.inverse_at(i).is_some(), c != '\n');
                }
            }
        }
    }
}
