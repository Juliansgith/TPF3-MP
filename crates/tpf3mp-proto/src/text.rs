use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// UTF-8 text of at most `MAX` bytes that contains no control characters.
///
/// Deserialisation enforces both rules, so a decoded message never carries
/// oversized or terminal-hostile text into logs or the UI.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Text<const MAX: usize>(String);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TextError {
    #[error("text is {len} bytes; the limit is {max}")]
    TooLong { len: usize, max: usize },
    #[error("text contains a control character")]
    ControlCharacter,
}

impl<const MAX: usize> Text<MAX> {
    pub fn new(value: impl Into<String>) -> Result<Self, TextError> {
        Self::try_from(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<const MAX: usize> TryFrom<String> for Text<MAX> {
    type Error = TextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() > MAX {
            return Err(TextError::TooLong {
                len: value.len(),
                max: MAX,
            });
        }
        if value.chars().any(char::is_control) {
            return Err(TextError::ControlCharacter);
        }
        Ok(Self(value))
    }
}

impl<const MAX: usize> From<Text<MAX>> for String {
    fn from(text: Text<MAX>) -> Self {
        text.0
    }
}

impl<const MAX: usize> fmt::Display for Text<MAX> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_unicode_within_the_byte_limit() {
        let text = Text::<8>::new("Jürgen").unwrap();
        assert_eq!(text.as_str(), "Jürgen");
    }

    #[test]
    fn limit_counts_bytes_not_characters() {
        // "ü" is two bytes, so this is 9 bytes in 8 characters.
        assert_eq!(
            Text::<8>::new("Jürgenxx"),
            Err(TextError::TooLong { len: 9, max: 8 })
        );
    }

    #[test]
    fn rejects_control_characters() {
        for value in ["line\nbreak", "tab\t", "esc\u{1b}[31m", "nul\0"] {
            assert_eq!(Text::<64>::new(value), Err(TextError::ControlCharacter));
        }
    }
}
