//! `hexcore` -- the editing engine behind the Rak hex editor.
//!
//! This crate has no GUI and no dependencies. It is the part that has to be right:
//! reading and writing bytes without loading a whole file into RAM, rendering rows a
//! human can scan, finding a byte sequence when part of it is unknown, undoing an
//! edit exactly, and saying *no* rather than guessing when a structure does not fit.
//!
//! ## Layout
//!
//! * [`document`] -- the bytes, a sparse overlay of pending edits, save, and undo.
//! * [`hexfmt`] -- offset / hex / text rows.
//! * [`search`] -- literal and wildcard patterns, over a document rather than a
//!   buffer, so a search reads through the overlay and windows a large file.
//! * [`structure`] -- field descriptions, bit fields, magic-value scanning.
//! * [`undo`] -- the history, kept separate from the document.
//!
//! ## The two rules the rest follows from
//!
//! **Reads see pending edits.** A hex editor that shows the file rather than what
//! you have done is worse than useless: you type, nothing appears, and you cannot
//! tell whether the key did not register or the view is stale. Every read here goes
//! through [`Document::byte`], [`Document::slice`] or [`Document::read_into`], which
//! consult the overlay first.
//!
//! **A refusal is a value.** Out-of-bounds, read-only, a pattern with a lone nibble,
//! a structure whose magic does not match -- each is a specific reason the caller can
//! show, not a string to unwrap. `Error` is an enum for that reason.

#![deny(missing_debug_implementations)]

pub mod document;
pub mod hexfmt;
pub mod search;
pub mod structure;
pub mod undo;

pub use document::{Document, Error, Result, Span};
pub use hexfmt::{format_row, format_span, DEFAULT_WIDTH};
pub use search::{BytePattern, Pattern};
pub use structure::{Decoded, Endian, Field, Kind, Layout, Value};

/// A structure description in the compact form the CLI and the GUI's structure
/// editor both take.
///
/// Kept as a string format rather than a builder type because it is what gets typed,
/// pasted between sessions, and stored in a project's file. The grammar is the
/// smallest one that covers the cases worth describing:
///
/// ```text
/// u8/u16le/u16be/u32le/u32be/u64le/u64be   unsigned
/// i8/i16le/i16be/i32le/i32be/i64le/i64be   signed
/// bits:<n>                                n bits from the next byte, low end
/// bytes:<n>                               n raw bytes
/// magic:<hex>                             must match exactly
/// name=value                              a field; the name is the part before `=`
/// <name>:<...>                            a nested group
/// ```
///
/// A field with no `=` gets the name `field<N>`, so a layout can be written as bare
/// types when the names do not matter.
pub fn parse_layout(text: &str) -> std::result::Result<Layout, String> {
    use structure::{Endian, Field};
    let mut fields: Vec<Field> = Vec::new();
    let mut counter = 0usize;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (name, spec) = match line.split_once('=') {
            Some((n, s)) => (n.trim().to_string(), s.trim().to_string()),
            None => {
                counter += 1;
                (format!("field{counter}"), line.to_string())
            }
        };
        let (ty, arg) = match spec.split_once(':') {
            Some((t, a)) => (t.trim(), Some(a.trim())),
            None => (spec.as_str(), None),
        };
        let unsigned = |ty: &str| -> Option<(usize, Endian)> {
            Some(match ty {
                "u8" => (1, Endian::Little),
                "u16le" => (2, Endian::Little),
                "u16be" => (2, Endian::Big),
                "u32le" => (4, Endian::Little),
                "u32be" => (4, Endian::Big),
                "u64le" => (8, Endian::Little),
                "u64be" => (8, Endian::Big),
                _ => return None,
            })
        };
        let signed = |ty: &str| -> Option<(usize, Endian)> {
            Some(match ty {
                "i8" => (1, Endian::Little),
                "i16le" => (2, Endian::Little),
                "i16be" => (2, Endian::Big),
                "i32le" => (4, Endian::Little),
                "i32be" => (4, Endian::Big),
                "i64le" => (8, Endian::Little),
                "i64be" => (8, Endian::Big),
                _ => return None,
            })
        };
        if let Some((size, endian)) = unsigned(ty) {
            fields.push(Field::Uint {
                name,
                size,
                endian,
            });
        } else if let Some((size, endian)) = signed(ty) {
            fields.push(Field::Int {
                name,
                size,
                endian,
            });
        } else if ty == "bits" {
            let n: u32 = arg
                .ok_or_else(|| format!("`bits` needs a width: `bits:4` (in `{line}`)"))?
                .parse()
                .map_err(|_| format!("`{}` is not a bit count", arg.unwrap_or("")))?;
            if n == 0 || n > 32 {
                return Err(format!("`bits:{n}` must be 1..=32"));
            }
            fields.push(Field::Bits {
                name,
                len: n,
                size: 1,
                endian: Endian::Big,
            });
        } else if ty == "bytes" {
            let n: usize = arg
                .ok_or_else(|| format!("`bytes` needs a length: `bytes:4` (in `{line}`)"))?
                .parse()
                .map_err(|_| format!("`{}` is not a length", arg.unwrap_or("")))?;
            if n == 0 || n > 4096 {
                return Err(format!("`bytes:{n}` must be 1..=4096"));
            }
            fields.push(Field::Bytes { name, len: n });
        } else if ty == "magic" {
            let hex = arg.ok_or_else(|| {
                format!("`magic` needs hex digits: `magic:7f454c46` (in `{line}`)")
            })?;
            let cleaned: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
            if cleaned.is_empty() || cleaned.len() % 2 != 0 {
                return Err(format!("`magic:{hex}` is not whole bytes of hex"));
            }
            let mut bytes = Vec::with_capacity(cleaned.len() / 2);
            for pair in cleaned.as_bytes().chunks(2) {
                let s = std::str::from_utf8(pair).map_err(|_| "bad hex".to_string())?;
                bytes.push(
                    u8::from_str_radix(s, 16).map_err(|_| format!("`{s}` is not hex"))?,
                );
            }
            fields.push(Field::Magic { expected: bytes });
        } else if let Some(_inner) = arg {
            let nested = parse_layout(&format!("{}={}", name, spec))?;
            fields.push(Field::Struct {
                name,
                fields: nested.fields,
            });
        } else {
            return Err(format!(
                "`{ty}` is not a known type (in `{line}`); try u8, u16le, u32be, bits:4, bytes:4, magic:ff00, or a nested `name:...`"
            ));
        }
    }
    if fields.is_empty() {
        return Err("the layout has no fields".to_string());
    }

    // Name the structure after something useful. `# name: X` wins; otherwise the
    // first field line names it, so the heading is a field rather than whatever
    // comment happens to sit at the top of the file.
    let mut name: Option<String> = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        name = Some(line.split('=').next().unwrap_or(line).trim().to_string());
        break;
    }
    let mut label = name.unwrap_or_else(|| "layout".to_string());
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix('#') {
            if let Some(n) = rest.trim().strip_prefix("name:") {
                label = n.trim().to_string();
            }
        }
    }
    Ok(Layout::new(label, fields))
}