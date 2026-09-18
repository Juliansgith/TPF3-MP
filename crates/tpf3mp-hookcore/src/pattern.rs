//! IDA-style byte patterns and fast scanning.
//!
//! A pattern is written the way a disassembler prints it: two hex digits per
//! fixed byte and `??` (or `?`) for a byte that may be anything, e.g.
//! `48 8B ?? ?? E8`. Wildcards exist so a signature can skip the bytes that move
//! between builds - RIP-relative displacements, call targets, absolute
//! addresses - and match only the opcodes and operands that identify the code.

use std::fmt;

use thiserror::Error;

/// One byte of a [`Pattern`]: a fixed value or a wildcard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Nibblet {
    Fixed(u8),
    Any,
}

/// A parsed byte pattern, ready to scan with.
#[derive(Clone, PartialEq, Eq)]
pub struct Pattern {
    bytes: Vec<Nibblet>,
    /// Index of the first fixed byte, used to anchor the scan. `None` only for
    /// an all-wildcard pattern, which [`Pattern::parse`] rejects.
    anchor: usize,
    anchor_byte: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PatternError {
    #[error("pattern is empty")]
    Empty,
    #[error("pattern is all wildcards, so it would match everywhere")]
    AllWildcards,
    #[error("token {token:?} is not two hex digits or a `??` wildcard")]
    BadToken { token: String },
}

/// Why a unique scan did not find exactly one match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ScanError {
    #[error("no match")]
    NotFound,
    #[error("{count} matches; the signature is not unique")]
    Ambiguous { count: usize },
}

impl Pattern {
    /// Parses an IDA-style pattern. Tokens are whitespace-separated; each is two
    /// hex digits or `?`/`??`. At least one token must be fixed.
    pub fn parse(text: &str) -> Result<Self, PatternError> {
        let mut bytes = Vec::new();
        for token in text.split_whitespace() {
            let nibblet = if token == "?" || token == "??" {
                Nibblet::Any
            } else if token.len() == 2 {
                let value = u8::from_str_radix(token, 16).map_err(|_| PatternError::BadToken {
                    token: token.to_owned(),
                })?;
                Nibblet::Fixed(value)
            } else {
                return Err(PatternError::BadToken {
                    token: token.to_owned(),
                });
            };
            bytes.push(nibblet);
        }
        if bytes.is_empty() {
            return Err(PatternError::Empty);
        }
        let anchor = bytes
            .iter()
            .position(|n| matches!(n, Nibblet::Fixed(_)))
            .ok_or(PatternError::AllWildcards)?;
        let anchor_byte = match bytes[anchor] {
            Nibblet::Fixed(value) => value,
            Nibblet::Any => unreachable!("anchor points at a fixed byte"),
        };
        Ok(Self {
            bytes,
            anchor,
            anchor_byte,
        })
    }

    /// The pattern's length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Whether the pattern matches `haystack` starting exactly at `pos`.
    pub fn matches_at(&self, haystack: &[u8], pos: usize) -> bool {
        let Some(window) = haystack.get(pos..pos + self.bytes.len()) else {
            return false;
        };
        self.bytes
            .iter()
            .zip(window)
            .all(|(nibblet, &byte)| match nibblet {
                Nibblet::Fixed(value) => *value == byte,
                Nibblet::Any => true,
            })
    }

    /// Every start offset in `haystack` where the pattern matches.
    ///
    /// The scan seeks the pattern's first fixed byte and only then checks the
    /// whole window, so an all-wildcard prefix costs nothing.
    pub fn find_all(&self, haystack: &[u8]) -> Vec<usize> {
        let mut hits = Vec::new();
        let width = self.bytes.len();
        if width > haystack.len() {
            return hits;
        }
        // Anchor positions: the first fixed byte must sit at `start + anchor`,
        // so the earliest start is 0 and the latest is haystack.len() - width.
        let last_start = haystack.len() - width;
        let mut probe = self.anchor;
        let last_probe = last_start + self.anchor;
        while probe <= last_probe {
            match memchr(self.anchor_byte, &haystack[probe..=last_probe]) {
                Some(rel) => {
                    let found = probe + rel;
                    let start = found - self.anchor;
                    if self.matches_at(haystack, start) {
                        hits.push(start);
                    }
                    probe = found + 1;
                }
                None => break,
            }
        }
        hits
    }

    /// The single offset where the pattern matches, or a [`ScanError`] when it
    /// matches zero or several times. Uniqueness is the whole point of a
    /// signature: a scanner that silently took the first of several matches
    /// could hook the wrong function on a patched build.
    pub fn find_unique(&self, haystack: &[u8]) -> Result<usize, ScanError> {
        let hits = self.find_all(haystack);
        match hits.as_slice() {
            [] => Err(ScanError::NotFound),
            [only] => Ok(*only),
            many => Err(ScanError::Ambiguous { count: many.len() }),
        }
    }
}

impl fmt::Debug for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pattern(\"")?;
        for (index, nibblet) in self.bytes.iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            match nibblet {
                Nibblet::Fixed(value) => write!(f, "{value:02X}")?,
                Nibblet::Any => f.write_str("??")?,
            }
        }
        f.write_str("\")")
    }
}

/// Index of the first `needle` byte in `haystack`.
///
/// A local, dependency-free equivalent of `memchr`: the scan runs over module
/// sections tens of megabytes wide, so anchoring on one byte before checking a
/// full window matters, but it is not worth a crate.
fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&byte| byte == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fixed_and_wildcard_tokens() {
        let pattern = Pattern::parse("48 8B ?? ?? E8").unwrap();
        assert_eq!(pattern.len(), 5);
    }

    #[test]
    fn accepts_single_question_mark_wildcard() {
        let pattern = Pattern::parse("48 ? E8").unwrap();
        assert!(pattern.matches_at(&[0x48, 0x99, 0xE8], 0));
    }

    #[test]
    fn rejects_malformed_patterns() {
        assert_eq!(Pattern::parse(""), Err(PatternError::Empty));
        assert_eq!(Pattern::parse("?? ??"), Err(PatternError::AllWildcards));
        assert!(matches!(
            Pattern::parse("48 8G"),
            Err(PatternError::BadToken { .. })
        ));
        assert!(matches!(
            Pattern::parse("488B"),
            Err(PatternError::BadToken { .. })
        ));
    }

    #[test]
    fn finds_all_occurrences() {
        let haystack = [0x90, 0x48, 0x8B, 0x01, 0x48, 0x8B, 0x02, 0x48, 0x8B, 0x03];
        let pattern = Pattern::parse("48 8B ??").unwrap();
        assert_eq!(pattern.find_all(&haystack), vec![1, 4, 7]);
    }

    #[test]
    fn wildcards_match_any_byte_in_that_position() {
        let pattern = Pattern::parse("E8 ?? ?? ?? ??").unwrap();
        assert!(pattern.matches_at(&[0xE8, 0x00, 0x11, 0x22, 0x33], 0));
        assert!(pattern.matches_at(&[0xE8, 0xFF, 0xFF, 0xFF, 0xFF], 0));
        assert!(!pattern.matches_at(&[0xE9, 0x00, 0x11, 0x22, 0x33], 0));
    }

    #[test]
    fn unique_reports_not_found_and_ambiguous() {
        let haystack = [0x48, 0x8B, 0xC0, 0x48, 0x8B, 0xC0];
        let unique = Pattern::parse("48 8B C0").unwrap();
        assert_eq!(
            unique.find_unique(&haystack),
            Err(ScanError::Ambiguous { count: 2 })
        );
        let absent = Pattern::parse("CC CC CC").unwrap();
        assert_eq!(absent.find_unique(&haystack), Err(ScanError::NotFound));
        let once = Pattern::parse("8B C0 48").unwrap();
        assert_eq!(once.find_unique(&haystack), Ok(1));
    }

    #[test]
    fn anchor_skips_wildcard_prefix() {
        // A leading wildcard must not anchor the scan on offset 0.
        let haystack = [0x00, 0x11, 0xE8, 0x22];
        let pattern = Pattern::parse("?? E8").unwrap();
        assert_eq!(pattern.find_all(&haystack), vec![1]);
    }

    #[test]
    fn pattern_longer_than_haystack_never_matches() {
        let pattern = Pattern::parse("48 8B C0 90").unwrap();
        assert!(pattern.find_all(&[0x48, 0x8B]).is_empty());
    }
}
