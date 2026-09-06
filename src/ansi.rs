//! The escape-sequence stripper the plain views of a run's output go through.
//!
//! A run writes to a pseudo-terminal, so its output carries the same escape
//! sequences a person would see in a terminal. [`plain`] is the one place in
//! the crate that knows their grammar: it removes them from the copy the farm
//! and the completion tail receive, and from every line `rxd` prints when its
//! own stdout is not a terminal.

const ESC: char = '\u{1b}';
const BEL: char = '\u{7}';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Text,
    Escape,
    CsiParameters,
    CsiIntermediates,
    Osc,
    OscEscape,
}

fn is_parameter(character: char) -> bool {
    ('\u{30}'..='\u{3f}').contains(&character)
}

fn is_intermediate(character: char) -> bool {
    ('\u{20}'..='\u{2f}').contains(&character)
}

fn is_final(character: char) -> bool {
    ('\u{40}'..='\u{7e}').contains(&character)
}

/// Returns `text` with every escape sequence removed.
///
/// A CSI sequence, an OSC sequence ended by `BEL` or `ST`, and a bare escape
/// followed by one character all disappear; an escape whose sequence does not
/// complete before the end of `text` takes the rest of `text` with it.
/// Everything else, including tabs, carriage returns and non-ASCII characters,
/// comes back as it was written.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::ansi::plain;
///
/// assert_eq!(plain("\u{1b}[32mok\u{1b}[0m"), "ok");
/// assert_eq!(plain("no sequences here"), "no sequences here");
/// ```
#[must_use]
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut state = State::Text;
    for character in text.chars() {
        match state {
            State::Text => {
                if character == ESC {
                    state = State::Escape;
                } else {
                    out.push(character);
                }
            }
            State::Escape => {
                if character == '[' {
                    state = State::CsiParameters;
                } else if character == ']' {
                    state = State::Osc;
                } else if character == ESC {
                    state = State::Escape;
                } else {
                    state = State::Text;
                }
            }
            State::CsiParameters => {
                if is_parameter(character) {
                    state = State::CsiParameters;
                } else if is_intermediate(character) {
                    state = State::CsiIntermediates;
                } else if is_final(character) {
                    state = State::Text;
                } else if character == ESC {
                    state = State::Escape;
                } else {
                    state = State::Text;
                }
            }
            State::CsiIntermediates => {
                if is_intermediate(character) {
                    state = State::CsiIntermediates;
                } else if is_final(character) {
                    state = State::Text;
                } else if character == ESC {
                    state = State::Escape;
                } else {
                    state = State::Text;
                }
            }
            State::Osc => {
                if character == BEL {
                    state = State::Text;
                } else if character == ESC {
                    state = State::OscEscape;
                } else {
                    state = State::Osc;
                }
            }
            State::OscEscape => {
                if character == '\\' {
                    state = State::Text;
                } else if character == ESC {
                    state = State::OscEscape;
                } else {
                    state = State::Osc;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::plain;

    #[test]
    fn every_shape_of_input_strips_to_its_plain_text() {
        let cases = [
            ("no sequences here", "no sequences here"),
            ("\u{1b}[32mgreen\u{1b}[0m", "green"),
            (
                "\u{1b}[32mgreen\u{1b}[0m and \u{1b}[31mred\u{1b}[0m",
                "green and red",
            ),
            ("\u{1b}[1;31;40mloud\u{1b}[0m", "loud"),
            ("cleared\u{1b}[K", "cleared"),
            ("wiped\u{1b}[2J", "wiped"),
            ("title\u{1b}]0;a window\u{7}stays", "titlestays"),
            ("title\u{1b}]0;a window\u{1b}\\stays", "titlestays"),
            ("bare\u{1b}Mescape", "bareescape"),
            ("twice\u{1b}\u{1b}[31mred\u{1b}[0m", "twicered"),
            ("cut here\u{1b}[3", "cut here"),
            ("kept\ttab\rreturn", "kept\ttab\rreturn"),
            ("привет 🌍 whole", "привет 🌍 whole"),
            ("\u{1b}[38;5;208mextended\u{1b}[0m", "extended"),
            ("\u{1b}[?25lhidden\u{1b}[?25h", "hidden"),
        ];
        for (input, expected) in cases {
            assert_eq!(plain(input), expected, "input {input:?}");
        }
    }

    #[test]
    fn input_that_carries_no_text_strips_to_nothing() {
        let cases = ["", "\u{1b}", "\u{1b}[0m", "\u{1b}]0;title\u{7}", "\u{1b}["];
        for input in cases {
            assert_eq!(plain(input), "", "input {input:?}");
        }
    }

    #[test]
    fn a_truncated_sequence_takes_the_rest_of_the_line_with_it() {
        assert_eq!(plain("start\u{1b}]0;never ends"), "start");
        assert_eq!(plain("start\u{1b}[3;2"), "start");
    }
}
