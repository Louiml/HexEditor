//! Pattern search, including the wildcard form a hex editor actually needs.
//!
//! A hex editor's search box takes one of two things:
//!
//! * **Hex digits**, because the user is looking at a hex dump and copying from it.
//!   `de ad be ef` is four bytes; `deadbeef` is the same four bytes, because nobody
//!   types spaces when copying a selection.
//! * **Wildcards**, because the byte you are hunting is the one you do not know.
//!   Finding the `7F 45 4C 46` magic with an unknown third byte means `7f ?? 4c 46`,
//!   and nibble wildcards (`?` matching half a byte) are what you need when only
//!   half a byte is known.
//!
//! Both spellings are parsed by [`Pattern::parse`], which decides between them from
//! the characters used rather than from a mode flag -- a user pasting `de ad` should
//! not have to say "I mean hex".

use crate::document::{Document, Error, Result};

/// One byte position in a pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BytePattern {
    /// Must equal this byte.
    Literal(u8),
    /// Any value.
    Any,
    /// The high nibble must match; the low nibble is unconstrained.
    HighNibble(u8),
    /// The low nibble must match; the high nibble is unconstrained.
    LowNibble(u8),
}

impl BytePattern {
    fn matches(&self, b: u8) -> bool {
        match *self {
            BytePattern::Literal(want) => b == want,
            BytePattern::Any => true,
            BytePattern::HighNibble(n) => b >> 4 == n,
            BytePattern::LowNibble(n) => b & 0x0F == n,
        }
    }
}

/// A parsed search pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    bytes: Vec<BytePattern>,
    /// True when at least one position is unconstrained, so a match cannot be found
    /// by comparing whole words.
    has_wildcards: bool,
}

impl Pattern {
    /// Parse a pattern written either way.
    ///
    /// Accepts `??`, `?` (nibble), and hex, with or without spaces. A `.` is an
    /// alias for `?` because it is what a hex editor's own UI tends to show.
    ///
    /// An empty pattern is an error rather than a match-everything: searching for
    /// nothing should say so, not return the whole file.
    pub fn parse(text: &str) -> Result<Pattern> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(Error::Pattern("the pattern is empty".to_string()));
        }

        // Nibble-level parse: every other character must be a hex digit or `?`.
        let chars: Vec<char> = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
        if chars.is_empty() {
            return Err(Error::Pattern("the pattern is empty".to_string()));
        }
        for c in &chars {
            if !c.is_ascii_hexdigit() && *c != '?' && *c != '.' {
                return Err(Error::Pattern(format!(
                    "'{}' is not a hex digit, `?` or `.`",
                    c
                )));
            }
        }
        if chars.len() % 2 != 0 {
            return Err(Error::Pattern(format!(
                "a hex pattern needs an even number of nibbles, got {} ('{}' is a lone nibble)",
                chars.len(),
                trimmed
            )));
        }

        let mut bytes = Vec::with_capacity(chars.len() / 2);
        let mut has_wildcards = false;
        for pair in chars.chunks(2) {
            let hi = nibble(pair[0], trimmed)?;
            let lo = nibble(pair[1], trimmed)?;
            let byte = match (hi, lo) {
                (Some(h), Some(l)) => BytePattern::Literal(h << 4 | l),
                (Some(h), None) => {
                    has_wildcards = true;
                    BytePattern::HighNibble(h)
                }
                (None, Some(l)) => {
                    has_wildcards = true;
                    BytePattern::LowNibble(l)
                }
                (None, None) => {
                    has_wildcards = true;
                    BytePattern::Any
                }
            };
            bytes.push(byte);
        }

        Ok(Pattern {
            bytes,
            has_wildcards,
        })
    }

    /// A pattern of exactly these bytes, no wildcards.
    pub fn literal(bytes: &[u8]) -> Pattern {
        Pattern {
            bytes: bytes.iter().map(|b| BytePattern::Literal(*b)).collect(),
            has_wildcards: false,
        }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    pub fn has_wildcards(&self) -> bool {
        self.has_wildcards
    }

    /// Does this pattern match at `offset` in `haystack`?
    ///
    /// # Panics
    /// If `offset + self.len()` runs past the end of `haystack`.
    pub fn matches_at(&self, haystack: &[u8], offset: usize) -> bool {
        self.bytes
            .iter()
            .enumerate()
            .all(|(i, p)| p.matches(haystack[offset + i]))
    }

    /// Find every match in `haystack`, ascending.
    pub fn find_all(&self, haystack: &[u8]) -> Vec<u64> {
        let mut out = Vec::new();
        if self.len() > haystack.len() {
            return out;
        }
        for start in 0..=(haystack.len() - self.len()) {
            if self.matches_at(haystack, start) {
                out.push(start as u64);
            }
        }
        out
    }

    /// The most selective position, for skipping during a scan.
    ///
    /// A literal is worth far more than a wildcard for rejecting a candidate, so
    /// this prefers one. It is the whole reason a wildcard search over a large file
    /// does not have to be quadratic in practice.
    fn anchor(&self) -> Option<(usize, u8)> {
        self.bytes.iter().enumerate().find_map(|(i, p)| match p {
            BytePattern::Literal(b) => Some((i, *b)),
            _ => None,
        })
    }

    /// Find matches in a document, reading through the overlay.
    ///
    /// Reads the document in windows rather than the whole file: a wildcard search
    /// over a 2 GB image must not need 2 GB of RAM. Windows overlap by
    /// `pattern.len() - 1` so a match straddling a window boundary is still found.
    pub fn find_in(&self, doc: &Document, limit: usize) -> Result<Vec<u64>> {
        let plen = self.len();
        let doc_len = doc.len();
        if plen == 0 || plen as u64 > doc_len {
            return Ok(Vec::new());
        }
        const WINDOW: u64 = 1 << 20; // 1 MiB
        let overlap = (plen - 1) as u64;
        let anchor = self.anchor();

        let mut found: Vec<u64> = Vec::new();
        let mut base = 0u64;

        while base < doc_len {
            let end = (base + WINDOW + overlap).min(doc_len);
            let buf = doc.slice(base, end)?;
            let blen = buf.len();

            // Candidate starts within this window. `base > 0` skips offset 0 of the
            // window, because a match starting there was the previous window's
            // business -- that is what the overlap is for, and it is what stops a
            // pattern being reported once per window.
            let mut i: usize = usize::from(base > 0);

            'outer: while i + plen <= blen {
                let candidate = match anchor {
                    // Skip to the next byte that could satisfy the anchor position
                    // rather than testing every offset. This is the difference
                    // between a wildcard search being usable on a large file and not.
                    Some((pos, want)) => {
                        let target = pos;
                        let mut j = i + target;
                        // `c + plen <= blen` with `c = j - target`, so the last
                        // legal j is `blen - plen + target` *inclusive*.
                        let last = blen - plen + target;
                        let mut hit = None;
                        while j <= last {
                            if buf[j] == want {
                                hit = Some(j - target);
                                break;
                            }
                            j += 1;
                        }
                        match hit {
                            Some(at) => at,
                            // No candidate byte left in this window.
                            None => break 'outer,
                        }
                    }
                    None => i,
                };

                if self.matches_at(&buf, candidate) {
                    let abs = base + candidate as u64;
                    if found.last() != Some(&abs) {
                        found.push(abs);
                        if found.len() >= limit {
                            return Ok(found);
                        }
                    }
                    // Step past this match rather than restarting the anchor scan:
                    // otherwise a pattern of repeated bytes reports only every
                    // other overlap.
                    i = candidate + 1;
                } else {
                    i = candidate + 1;
                }
            }

            if end >= doc_len {
                break;
            }
            base += WINDOW;
        }
        Ok(found)
    }
}

fn nibble(c: char, whole: &str) -> Result<Option<u8>> {
    if c == '?' || c == '.' {
        return Ok(None);
    }
    c.to_digit(16)
        .map(|d| Some(d as u8))
        .ok_or_else(|| Error::Pattern(format!("'{c}' in '{whole}' is not a hex digit")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaced_and_unspaced_hex_parse_the_same() {
        assert_eq!(
            Pattern::parse("de ad be ef").unwrap(),
            Pattern::parse("deadbeef").unwrap()
        );
        assert_eq!(
            Pattern::parse("deadbeef").unwrap(),
            Pattern::literal(&[0xDE, 0xAD, 0xBE, 0xEF])
        );
    }

    #[test]
    fn lowercase_and_uppercase_hex_agree() {
        assert_eq!(Pattern::parse("DeAd").unwrap(), Pattern::parse("dead").unwrap());
    }

    #[test]
    fn an_empty_pattern_is_refused() {
        assert!(Pattern::parse("").is_err());
        assert!(Pattern::parse("   ").is_err());
    }

    #[test]
    fn a_lone_nibble_is_refused_by_name() {
        let err = Pattern::parse("dea").unwrap_err();
        assert!(
            format!("{err}").contains("even number of nibbles"),
            "got {err}"
        );
    }

    #[test]
    fn a_non_hex_character_is_refused() {
        assert!(Pattern::parse("zz").is_err());
        assert!(Pattern::parse("de ad zz").is_err());
    }

    #[test]
    fn a_full_byte_wildcard_matches_anything() {
        let p = Pattern::parse("7f ?? 4c 46").unwrap();
        assert_eq!(p.len(), 4);
        assert!(p.has_wildcards());
        assert!(p.matches_at(&[0x7F, 0x00, 0x4C, 0x46], 0));
        assert!(p.matches_at(&[0x7F, 0xFF, 0x4C, 0x46], 0));
        assert!(!p.matches_at(&[0x7F, 0x00, 0x4C, 0x47], 0));
        assert!(!p.matches_at(&[0x70, 0x00, 0x4C, 0x46], 0));
    }

    #[test]
    fn a_half_byte_wildcard_constrains_one_nibble() {
        let p = Pattern::parse("7f?4").unwrap();
        assert_eq!(p.len(), 2);
        assert!(p.matches_at(&[0x7F, 0x04], 0));
        assert!(p.matches_at(&[0x7F, 0xF4], 0));
        assert!(!p.matches_at(&[0x7F, 0x05], 0), "low nibble must be 4");
        assert!(!p.matches_at(&[0x70, 0xF4], 0), "high nibble must be 7");

        let p = Pattern::parse("?f").unwrap();
        assert!(p.matches_at(&[0x0F], 0));
        assert!(p.matches_at(&[0xAF], 0));
        assert!(!p.matches_at(&[0xF0], 0), "low nibble must be f");
    }

    #[test]
    fn a_dot_is_accepted_as_a_nibble_wildcard() {
        // `.` is an alias for `?`, which is one *nibble*, so an odd number of
        // characters is still an error.
        assert_eq!(
            Pattern::parse("de..ef").unwrap(),
            Pattern::parse("de??ef").unwrap()
        );
        assert!(Pattern::parse("de.ef").is_err(), "five nibbles is not whole bytes");
    }

    #[test]
    fn find_all_returns_every_overlapping_match_ascending() {
        let p = Pattern::literal(b"aaa");
        let hits = p.find_all(b"aaaaa");
        assert_eq!(hits, vec![0, 1, 2], "overlaps must all be reported");
    }

    #[test]
    fn find_all_on_a_short_haystack_is_empty() {
        assert!(Pattern::literal(b"abcd").find_all(b"abc").is_empty());
        assert!(Pattern::literal(b"abcd").find_all(b"").is_empty());
    }

    #[test]
    fn document_search_sees_the_overlay_not_just_the_file() {
        let mut doc = Document::from_bytes(b"\x00\x00\x00\x00".to_vec());
        doc.poke(2, 0xAB).unwrap();
        let hits = Pattern::literal(&[0xAB]).find_in(&doc, 100).unwrap();
        assert_eq!(hits, vec![2], "the pending edit must be searchable");
    }

    #[test]
    fn document_search_finds_wildcards() {
        let doc = Document::from_bytes(vec![0x7F, 0x01, 0x4C, 0x46, 0x7F, 0x02, 0x4C, 0x46]);
        let hits = Pattern::parse("7f ?? 4c 46").unwrap().find_in(&doc, 100).unwrap();
        assert_eq!(hits, vec![0, 4]);
    }

    #[test]
    fn a_match_straddling_a_window_boundary_is_found() {
        // Force many windows with a small one, by searching a pattern longer than
        // the window stride would allow is not possible here, so instead make the
        // document span several windows and put a match at the seam.
        let mut doc = Document::from_bytes(vec![0u8; (1 << 20) * 2 + 64]);
        let boundary = (1 << 20) + 10;
        let needle = Pattern::literal(&[0xDE, 0xAD, 0xBE, 0xEF]);
        // Write the needle so it spans `base + window`.
        doc.poke(boundary - 2, 0xDE).unwrap();
        doc.poke(boundary - 1, 0xAD).unwrap();
        doc.poke(boundary, 0xBE).unwrap();
        doc.poke(boundary + 1, 0xEF).unwrap();
        let hits = needle.find_in(&doc, 100).unwrap();
        assert_eq!(hits, vec![boundary - 2], "the seam match was missed");
    }

    #[test]
    fn the_limit_is_respected() {
        let doc = Document::from_bytes(vec![0x41u8; 1000]);
        let hits = Pattern::literal(&[0x41]).find_in(&doc, 10).unwrap();
        assert_eq!(hits.len(), 10);
    }

    #[test]
    fn a_pattern_longer_than_the_document_matches_nothing() {
        let doc = Document::from_bytes(vec![1, 2, 3]);
        assert!(Pattern::literal(&[1, 2, 3, 4]).find_in(&doc, 10).unwrap().is_empty());
    }

    #[test]
    fn an_all_wildcard_pattern_of_n_bytes_finds_every_offset() {
        let doc = Document::from_bytes(vec![9u8; 6]);
        let hits = Pattern::parse("?? ??").unwrap().find_in(&doc, 100).unwrap();
        assert_eq!(hits, vec![0, 1, 2, 3, 4], "5 windows of 2 in 6 bytes");
    }
}