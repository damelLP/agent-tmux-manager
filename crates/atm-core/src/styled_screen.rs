//! Strict, vendor-neutral decoding of tmux's styled live viewport.
//!
//! This is not a terminal emulator. Only the captured SGR subset and OSC-8 link
//! metadata are accepted; unknown controls invalidate the entire observation.
//! Offsets address UTF-8 character starts, never terminal cells. Spaces and LF
//! are retained exactly, including tmux's allocated grid padding.

#[derive(Debug, Clone, Copy, Default)]
struct Style {
    bold: bool,
    faint: bool,
    inverse: bool,
}

/// Decoded viewport text and attributes at UTF-8 character-start byte offsets.
#[derive(Debug, Clone)]
pub struct StyledScreen {
    text: String,
    styles: Vec<(usize, Style)>,
}

/// Unsupported or malformed terminal control syntax invalidates the whole screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("unsupported or malformed captured terminal text")]
pub struct InvalidStyledScreen;

impl StyledScreen {
    /// Decode supported SGR/OSC-8 controls, preserving every visible character and LF.
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

    /// Return decoded text without trimming spaces or line breaks.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Read bold at a decoded UTF-8 character start; LF, non-boundary and invalid offsets give None.
    #[must_use]
    pub fn bold_at(&self, byte_offset: usize) -> Option<bool> {
        self.style_at(byte_offset).map(|s| s.bold)
    }

    /// Read faint at a decoded UTF-8 character start; LF, non-boundary and invalid offsets give None.
    #[must_use]
    pub fn faint_at(&self, byte_offset: usize) -> Option<bool> {
        self.style_at(byte_offset).map(|s| s.faint)
    }

    /// Read inverse at a decoded UTF-8 character start; LF, non-boundary and invalid offsets give None.
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
                1 => self.bold = true,
                2 => self.faint = true,
                7 => self.inverse = true,
                22 => {
                    self.bold = false;
                    self.faint = false;
                }
                27 => self.inverse = false,
                // Italic, underline and colors do not affect the retained attributes.
                3 | 4 | 23 | 24 | 30..=37 | 39 | 40..=47 | 49 | 90..=97 | 100..=107 => {}
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
    fn underline_preserves_text_and_other_attributes() {
        let screen = StyledScreen::parse("\x1b[1;2;7mA\x1b[4m界\x1b[24mZ\x1b[0m!\n")
            .expect("underline toggles are presentation only");
        assert_eq!(screen.text(), "A界Z!\n");
        for offset in [0, 1, 4, 5] {
            let expected = Some(offset != 5);
            assert_eq!(screen.bold_at(offset), expected);
            assert_eq!(screen.faint_at(offset), expected);
            assert_eq!(screen.inverse_at(offset), expected);
        }
        for offset in [2, 3, 6] {
            assert_eq!(screen.bold_at(offset), None);
            assert_eq!(screen.faint_at(offset), None);
            assert_eq!(screen.inverse_at(offset), None);
        }
        assert_eq!(StyledScreen::parse("A\x1b[4;24mB").unwrap().text(), "AB");
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
            "\x1b[4:2m",
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

    #[test]
    fn bold_state_and_intensity_resets() {
        let s = StyledScreen::parse(
            "\x1b[1mA\x1b[2mB\x1b[22mC\x1b[1;2mD\x1b[0mE\x1b[1mF\x1b[mG\x1b[22;1mH\x1b[1;22mI\x1b[2;1mJ",
        )
        .unwrap();
        assert_eq!(s.text(), "ABCDEFGHIJ");
        for (offset, bold, faint) in [
            (0, true, false),
            (1, true, true),
            (2, false, false),
            (3, true, true),
            (4, false, false),
            (5, true, false),
            (6, false, false),
            (7, true, false),
            (8, false, false),
            (9, true, true),
        ] {
            assert_eq!(s.bold_at(offset), Some(bold));
            assert_eq!(s.faint_at(offset), Some(faint));
        }
    }

    #[test]
    fn bold_ignores_color_components() {
        let s = StyledScreen::parse(
            "\x1b[38;2;1;2;7;48;5;1mA\x1b[1;38;5;22mB\x1b[48;2;22;0;0mC\x1b[39;49mD",
        )
        .unwrap();
        assert_eq!(s.text(), "ABCD");
        for (offset, bold) in [(0, false), (1, true), (2, true), (3, true)] {
            assert_eq!(s.bold_at(offset), Some(bold));
        }
    }

    #[test]
    fn bold_uses_character_starts_and_survives_line_breaks() {
        let s = StyledScreen::parse("\x1b[1m界é \nZ\x1b[22m!").unwrap();
        assert_eq!(s.text(), "界é \nZ!");
        for offset in [0, 3, 5, 7] {
            assert_eq!(s.bold_at(offset), Some(true));
        }
        assert_eq!(s.bold_at(8), Some(false));
        for offset in [1, 2, 4, 6, 9, usize::MAX] {
            assert_eq!(s.bold_at(offset), None);
        }
    }

    #[test]
    fn osc8_preserves_bold_label() {
        for end in ["\x07", "\x1b\\"] {
            let s = StyledScreen::parse(&format!(
                "\x1b[1m\x1b]8;id=x;https://example.test{end}link\x1b]8;;{end}X",
            ))
            .unwrap();
            assert_eq!(s.text(), "linkX");
            for offset in 0..5 {
                assert_eq!(s.bold_at(offset), Some(true));
            }
        }
    }

    // Independent reference for accepted streams: walk characters, skip link metadata,
    // consume complete color groups and track only the two observable attributes.
    type Reference = (String, Vec<(usize, bool, bool)>);

    fn reference(text: &str) -> Option<Reference> {
        let mut chars = text.chars();
        let (mut faint, mut inverse) = (false, false);
        let mut visible = String::new();
        let mut attributes = Vec::new();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                if c != '\n' {
                    attributes.push((visible.len(), faint, inverse));
                }
                visible.push(c);
                continue;
            }
            match chars.next()? {
                '[' => {
                    let parameters: String = chars.by_ref().take_while(|c| *c != 'm').collect();
                    if parameters.is_empty() {
                        (faint, inverse) = (false, false);
                        continue;
                    }
                    let mut values = parameters.split(';');
                    while let Some(value) = values.next() {
                        match value.parse::<u8>().ok()? {
                            0 => (faint, inverse) = (false, false),
                            2 => faint = true,
                            7 => inverse = true,
                            22 => faint = false,
                            27 => inverse = false,
                            38 | 48 => {
                                let components = if values.next()?.parse::<u8>().ok()? == 5 {
                                    1
                                } else {
                                    3
                                };
                                for _ in 0..components {
                                    values.next()?;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                ']' => loop {
                    match chars.next()? {
                        '\x07' => break,
                        '\x1b' if chars.next()? == '\\' => break,
                        _ => {}
                    }
                },
                _ => return None,
            }
        }
        Some((visible, attributes))
    }

    const VALID_FRAGMENTS: &[&str] = &[
        "A",
        "é界",
        " ",
        "\n",
        "\x1b[m",
        "\x1b[0m",
        "\x1b[2m",
        "\x1b[7m",
        "\x1b[22m",
        "\x1b[27m",
        "\x1b[1;2;3;7m",
        "\x1b[0;2m",
        "\x1b[38;5;2;7m",
        "\x1b[038;05;2;7m",
        "\x1b[48;2;7;7;7;2m",
        "\x1b[38;2;2;7;0m",
        "\x1b]8;id=x;https://example.test\x07",
        "\x1b]8;;\x1b\\",
    ];

    proptest! {
        #[test]
        fn fragment_alphabet_matches_reference(
            fragments in proptest::collection::vec(prop_oneof![
                proptest::sample::select(VALID_FRAGMENTS),
                proptest::sample::select(&[
                    "\x1b", "[", "]", "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
                    ";", ":", "m", "\x07", "\\", "\n", "text", "\r", "\u{9b}",
                ][..]),
            ], 0..128)
        ) {
            let text = fragments.concat();
            if let Ok(screen) = StyledScreen::parse(&text) {
                prop_assert!(!screen.text().chars().any(|c| c.is_control() && c != '\n'));
                let (visible, attributes) = reference(&text).unwrap();
                prop_assert_eq!(screen.text(), visible);
                for (offset, faint, inverse) in attributes {
                    prop_assert_eq!(screen.faint_at(offset), Some(faint));
                    prop_assert_eq!(screen.inverse_at(offset), Some(inverse));
                }
            }
        }

        #[test]
        fn valid_fragments_preserve_reference_attributes(
            fragments in proptest::collection::vec(proptest::sample::select(VALID_FRAGMENTS), 0..128)
        ) {
            let text = fragments.concat();
            let screen = StyledScreen::parse(&text).unwrap();
            let (visible, attributes) = reference(&text).unwrap();
            prop_assert_eq!(screen.text(), visible);
            for (offset, faint, inverse) in attributes {
                prop_assert_eq!(screen.faint_at(offset), Some(faint));
                prop_assert_eq!(screen.inverse_at(offset), Some(inverse));
            }
        }

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
