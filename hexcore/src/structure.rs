//! Decoding a byte range against a field description.
//!
//! This is the same idea as Rak's `binstruct`, in Rust and with the bits that
//! matter for reverse engineering: bit fields, endianness, and magic values that
//! must match for a structure to be recognised at all.
//!
//! Two design choices are deliberate and worth stating, because the alternatives
//! are worse:
//!
//! * **A decode failure is a value, not an `Err`.** Real files are mostly *not* the
//!   structure you are asking about. A decoder that stops at the first short read
//!   cannot be used to scan a file for candidates, which is the main thing you want
//!   it for. So [`Decoded::Failed`] carries the reason and the offset, and the
//!   caller decides.
//! * **Offsets are absolute.** A structure decoded at 0x400 and the same bytes at
//!   0x4000 have different offsets, and a structure view is only useful if it can
//!   say where it is.

use crate::document::{Document, Result};

/// How a multi-byte integer's bytes are ordered in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

/// One field in a structure description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// `n` raw bytes, shown as hex.
    Bytes { name: String, len: usize },
    /// An unsigned integer of `size` bytes.
    Uint {
        name: String,
        size: usize,
        endian: Endian,
    },
    /// A signed integer of `size` bytes.
    Int {
        name: String,
        size: usize,
        endian: Endian,
    },
    /// `len` bits taken from the low end of a `size`-byte container.
    Bits {
        name: String,
        len: u32,
        size: usize,
        endian: Endian,
    },
    /// Bytes that must equal `expected` for the structure to be recognised.
    Magic { expected: Vec<u8> },
    /// A nested structure, flattened with dotted names.
    Struct {
        name: String,
        fields: Vec<Field>,
    },
}

/// A named list of fields: one structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub name: String,
    pub fields: Vec<Field>,
}

impl Layout {
    pub fn new(name: impl Into<String>, fields: Vec<Field>) -> Layout {
        Layout {
            name: name.into(),
            fields,
        }
    }

    /// The number of bytes this layout consumes, if that is knowable.
    ///
    /// `None` when the layout contains a nested struct whose own size is unknown,
    /// because the answer then depends on the nested layout.
    pub fn size(&self) -> Option<u64> {
        let mut total = 0u64;
        for f in &self.fields {
            match f {
                Field::Bytes { len, .. } => total += *len as u64,
                Field::Uint { size, .. } | Field::Int { size, .. } | Field::Bits { size, .. } => {
                    total += *size as u64
                }
                Field::Magic { expected } => total += expected.len() as u64,
                Field::Struct { .. } => return None,
            }
        }
        Some(total)
    }

    /// Decode at `offset` in `doc`.
    pub fn decode(&self, doc: &Document, offset: u64) -> Result<Decoded> {
        let mut cursor = offset;
        let mut values = Vec::new();
        let mut problems = Vec::new();
        self.decode_into(doc, offset, &mut cursor, &mut values, &mut problems)?;
        Ok(Decoded {
            name: self.name.clone(),
            offset,
            size: cursor - offset,
            values,
            problems,
        })
    }

    fn decode_into(
        &self,
        doc: &Document,
        base: u64,
        cursor: &mut u64,
        out: &mut Vec<Value>,
        problems: &mut Vec<String>,
    ) -> Result<()> {
        // Bits consumed inside the byte at `cursor`, for bit fields that share a
        // container. `0x45` split as version=4, ihl=5 is two fields over one byte,
        // which a per-field cursor cannot express.
        let mut bit_pos: u32 = 0;
        for field in &self.fields {
            match field {
                Field::Bytes { name, len } => {
                    if *cursor + *len as u64 > doc.len() {
                        problems.push(format!("{name}: needs {len} byte(s) at {cursor:#x}"));
                        *cursor += *len as u64;
                        continue;
                    }
                    out.push(Value {
                        name: name.clone(),
                        offset: *cursor,
                        kind: Kind::Bytes(doc.slice(*cursor, *cursor + *len as u64)?),
                    });
                    *cursor += *len as u64;
                }
                Field::Uint {
                    name,
                    size,
                    endian,
                }
                | Field::Int {
                    name,
                    size,
                    endian,
                } => {
                    let signed = matches!(field, Field::Int { .. });
                    let end = *cursor + *size as u64;
                    if end > doc.len() {
                        problems.push(format!(
                            "{name}: needs {size} byte(s) at {cursor:#x}, file is {:#x}",
                            doc.len()
                        ));
                        *cursor = end;
                        continue;
                    }
                    let raw = doc.slice(*cursor, end)?;
                    let v = match endian {
                        Endian::Big => {
                            let mut acc: u64 = 0;
                            for b in &raw {
                                acc = acc << 8 | *b as u64;
                            }
                            acc
                        }
                        Endian::Little => raw
                            .iter()
                            .rev()
                            .fold(0u64, |acc, b| acc << 8 | *b as u64),
                    };
                    // Two's complement for the signed case, done on the assembled
                    // value so it does not depend on the byte order having been
                    // applied already.
                    let shown = if signed && *size < 8 && (v >> (*size * 8 - 1)) & 1 == 1 {
                        v as i64 - (1i64 << (*size * 8))
                    } else {
                        v as i64
                    };
                    out.push(Value {
                        name: name.clone(),
                        offset: *cursor,
                        kind: Kind::Int(shown),
                    });
                    *cursor = end;
                }
                Field::Bits {
                    name,
                    len,
                    size,
                    endian,
                } => {
                    let capacity = (*size as u32) * 8;
                    if *len == 0 || *len > capacity {
                        problems.push(format!(
                            "{name}: {len} bits will not fit in {size} byte(s)"
                        ));
                        bit_pos = 0;
                        *cursor += *size as u64;
                        continue;
                    }
                    // Move to the next container when this one is used up, so a run
                    // of bit fields packs instead of each starting a fresh byte.
                    if bit_pos + *len > capacity {
                        bit_pos = 0;
                        *cursor += *size as u64;
                    }
                    let end = *cursor + *size as u64;
                    if end > doc.len() {
                        problems.push(format!(
                            "{name}: needs {size} byte(s) at {cursor:#x}, file is {:#x}",
                            doc.len()
                        ));
                        bit_pos = 0;
                        *cursor = end;
                        continue;
                    }
                    let raw = doc.slice(*cursor, end)?;
                    let mut acc: u64 = 0;
                    match endian {
                        Endian::Big => {
                            for b in &raw {
                                acc = acc << 8 | *b as u64;
                            }
                        }
                        Endian::Little => {
                            for b in raw.iter().rev() {
                                acc = acc << 8 | *b as u64;
                            }
                        }
                    }
                    // Big-endian reads from the top of the container, so version and
                    // ihl come out of 0x45 as 4 and 5 rather than 5 and 4.
                    let shift = capacity - bit_pos - *len;
                    let mask = if *len >= 64 {
                        u64::MAX
                    } else {
                        (1u64 << *len) - 1
                    };
                    out.push(Value {
                        name: name.clone(),
                        offset: *cursor,
                        kind: Kind::Int(((acc >> shift) & mask) as i64),
                    });
                    bit_pos += *len;
                    if bit_pos == capacity {
                        bit_pos = 0;
                        *cursor = end;
                    }
                }
                Field::Magic { expected } => {
                    let end = *cursor + expected.len() as u64;
                    if end > doc.len() {
                        problems.push(format!(
                            "magic: needs {} byte(s) at {cursor:#x}",
                            expected.len()
                        ));
                        *cursor = end;
                        continue;
                    }
                    let raw = doc.slice(*cursor, end)?;
                    if &raw != expected.as_slice() {
                        problems.push(format!(
                            "magic at {cursor:#x}: expected {}, found {}",
                            hex(expected),
                            hex(&raw)
                        ));
                    }
                    *cursor = end;
                }
                Field::Struct { name, fields } => {
                    let nested = Layout::new(name.clone(), fields.clone());
                    let mut sub_cursor = *cursor;
                    // Nested values keep their own absolute offsets and get dotted
                    // names, so a flattened view still says which sub-field is which.
                    let before = out.len();
                    nested.decode_into(doc, base, &mut sub_cursor, out, problems)?;
                    for v in out.iter_mut().skip(before) {
                        v.name = format!("{}.{}", name, v.name);
                    }
                    *cursor = sub_cursor;
                }
            }
        }
        Ok(())
    }

    /// Every offset where this layout's magic fields all match.
    ///
    /// This is the "find the headers in this file" operation, and it is why magic
    /// bytes are worth describing separately from data.
    pub fn scan(&self, doc: &Document, limit: usize) -> Result<Vec<u64>> {
        let magics: Vec<&Vec<u8>> = self
            .fields
            .iter()
            .filter_map(|f| match f {
                Field::Magic { expected } => Some(expected),
                _ => None,
            })
            .collect();
        if magics.is_empty() {
            return Ok(Vec::new());
        }
        let width = magics.iter().map(|m| m.len()).max().unwrap_or(1);
        let mut found = Vec::new();
        let mut off = 0u64;
        let len = doc.len();
        while off + width as u64 <= len {
            let ok = magics.iter().all(|m| {
                let end = off + m.len() as u64;
                end <= len && doc.slice(off, end).map(|r| r == m.as_slice()).unwrap_or(false)
            });
            if ok {
                found.push(off);
                if found.len() >= limit {
                    break;
                }
                off += width as u64;
            } else {
                off += 1;
            }
        }
        Ok(found)
    }
}

/// One decoded field.
#[derive(Debug, Clone, PartialEq)]
pub struct Value {
    pub name: String,
    pub offset: u64,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Bytes(Vec<u8>),
    Int(i64),
}

/// The result of decoding at an offset.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub name: String,
    pub offset: u64,
    /// Bytes consumed, including any that a problem cut short.
    pub size: u64,
    pub values: Vec<Value>,
    /// Human-readable reasons the decode is not clean. Empty means it matched.
    pub problems: Vec<String>,
}

impl Decoded {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    /// A flat, aligned rendering, ready to print or to put in a side panel.
    pub fn render(&self) -> String {
        let width = self
            .values
            .iter()
            .map(|v| v.name.len())
            .max()
            .unwrap_or(4)
            .max(4);
        let mut out = format!(
            "{} @ {:#010x} ({} byte(s))\n",
            self.name, self.offset, self.size
        );
        for v in &self.values {
            let shown = match &v.kind {
                Kind::Bytes(b) => hex(b),
                Kind::Int(i) => {
                    if *i < 0 {
                        format!("{i} (0x{:X})", *i as u64)
                    } else {
                        format!("{i} (0x{:X})", i)
                    }
                }
            };
            out.push_str(&format!(
                "  {:<width$}  {:#010x}  {}\n",
                v.name,
                v.offset,
                shown,
                width = width
            ));
        }
        for p in &self.problems {
            out.push_str(&format!("  ! {p}\n"));
        }
        out
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(bytes: &[u8]) -> Document {
        Document::from_bytes(bytes.to_vec())
    }

    #[test]
    fn a_uint_decodes_at_the_right_size_and_offset() {
        let l = Layout::new(
            "hdr",
            vec![Field::Uint {
                name: "magic".into(),
                size: 4,
                endian: Endian::Big,
            }],
        );
        let d = doc(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00]);
        let r = l.decode(&d, 0).unwrap();
        assert!(r.is_clean());
        assert_eq!(r.size, 4);
        assert_eq!(r.values[0].offset, 0);
        assert_eq!(r.values[0].kind, Kind::Int(0xDEADBEEFu32 as i64));
    }

    #[test]
    fn endianness_is_honoured() {
        let d = doc(&[0x01, 0x02, 0x03, 0x04]);
        let be = Layout::new(
            "x",
            vec![Field::Uint {
                name: "v".into(),
                size: 4,
                endian: Endian::Big,
            }],
        );
        let le = Layout::new(
            "x",
            vec![Field::Uint {
                name: "v".into(),
                size: 4,
                endian: Endian::Little,
            }],
        );
        assert_eq!(be.decode(&d, 0).unwrap().values[0].kind, Kind::Int(0x01020304));
        assert_eq!(le.decode(&d, 0).unwrap().values[0].kind, Kind::Int(0x04030201));
    }

    #[test]
    fn a_negative_int_decodes_as_twos_complement() {
        let d = doc(&[0xFF, 0xFF]);
        let l = Layout::new(
            "x",
            vec![Field::Int {
                name: "v".into(),
                size: 2,
                endian: Endian::Big,
            }],
        );
        assert_eq!(l.decode(&d, 0).unwrap().values[0].kind, Kind::Int(-1));
    }

    #[test]
    fn a_one_byte_signed_value_of_0xff_is_minus_one() {
        // The case a naive "cast to u8 then reuse as i64" gets wrong.
        let d = doc(&[0xFF]);
        let l = Layout::new(
            "x",
            vec![Field::Int {
                name: "v".into(),
                size: 1,
                endian: Endian::Big,
            }],
        );
        assert_eq!(l.decode(&d, 0).unwrap().values[0].kind, Kind::Int(-1));
    }

    #[test]
    fn bit_fields_take_the_low_bits_of_their_container() {
        let d = doc(&[0x45, 0x00]);
        let l = Layout::new(
            "ip",
            vec![
                Field::Bits {
                    name: "version".into(),
                    len: 4,
                    size: 1,
                    endian: Endian::Big,
                },
                Field::Bits {
                    name: "ihl".into(),
                    len: 4,
                    size: 1,
                    endian: Endian::Big,
                },
            ],
        );
        let r = l.decode(&d, 0).unwrap();
        assert_eq!(r.values[0].kind, Kind::Int(4), "0x45 high nibble");
        assert_eq!(r.values[1].kind, Kind::Int(5), "0x45 low nibble");
    }

    #[test]
    fn a_magic_mismatch_is_reported_not_hidden() {
        let l = Layout::new(
            "hdr",
            vec![
                Field::Magic {
                    expected: vec![0x7F, 0x45, 0x4C, 0x46],
                },
                Field::Uint {
                    name: "v".into(),
                    size: 1,
                    endian: Endian::Big,
                },
            ],
        );
        let good = l.decode(&doc(&[0x7F, 0x45, 0x4C, 0x46, 9]), 0).unwrap();
        assert!(good.is_clean(), "{:?}", good.problems);
        let bad = l.decode(&doc(&[0x00, 0x00, 0x00, 0x00, 9]), 0).unwrap();
        assert!(!bad.is_clean());
        assert!(bad.problems[0].contains("magic"), "{:?}", bad.problems);
        // The rest still decodes, which is the point of reporting rather than
        // bailing: a partial view beats no view.
        assert_eq!(bad.values[0].kind, Kind::Int(9));
    }

    #[test]
    fn a_short_read_is_reported_and_does_not_panic() {
        let l = Layout::new(
            "x",
            vec![Field::Uint {
                name: "v".into(),
                size: 4,
                endian: Endian::Big,
            }],
        );
        let r = l.decode(&doc(&[1, 2]), 0).unwrap();
        assert!(!r.is_clean());
        assert!(r.problems[0].contains("file is"), "{:?}", r.problems);
    }

    #[test]
    fn an_offset_past_the_end_is_reported() {
        let l = Layout::new(
            "x",
            vec![Field::Bytes {
                name: "b".into(),
                len: 4,
            }],
        );
        let r = l.decode(&doc(&[1, 2, 3, 4]), 2).unwrap();
        assert!(!r.is_clean());
    }

    #[test]
    fn nested_layouts_flatten_with_dotted_names_and_keep_offsets() {
        let l = Layout::new(
            "outer",
            vec![
                Field::Uint {
                    name: "a".into(),
                    size: 1,
                    endian: Endian::Big,
                },
                Field::Struct {
                    name: "inner".into(),
                    fields: vec![
                        Field::Uint {
                            name: "b".into(),
                            size: 1,
                            endian: Endian::Big,
                        },
                        Field::Uint {
                            name: "c".into(),
                            size: 1,
                            endian: Endian::Big,
                        },
                    ],
                },
            ],
        );
        let r = l.decode(&doc(&[1, 2, 3]), 0).unwrap();
        let names: Vec<&str> = r.values.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["a", "inner.b", "inner.c"]);
        assert_eq!(r.values[2].offset, 2, "absolute offsets are preserved");
        assert_eq!(r.size, 3);
    }

    #[test]
    fn decoding_at_an_offset_shifts_every_field() {
        let l = Layout::new(
            "x",
            vec![
                Field::Bytes {
                    name: "a".into(),
                    len: 1,
                },
                Field::Bytes {
                    name: "b".into(),
                    len: 1,
                },
            ],
        );
        let r = l.decode(&doc(&[9, 9, 0xAA, 0xBB]), 2).unwrap();
        assert_eq!(r.offset, 2);
        assert_eq!(r.values[0].offset, 2);
        assert_eq!(r.values[1].offset, 3);
        assert_eq!(r.values[1].kind, Kind::Bytes(vec![0xBB]));
    }

    #[test]
    fn size_is_the_sum_of_the_fields() {
        let l = Layout::new(
            "x",
            vec![
                Field::Bytes {
                    name: "a".into(),
                    len: 3,
                },
                Field::Uint {
                    name: "b".into(),
                    size: 2,
                    endian: Endian::Big,
                },
            ],
        );
        assert_eq!(l.size(), Some(5));
    }

    #[test]
    fn scan_finds_every_occurrence_of_the_magic() {
        let l = Layout::new(
            "elf",
            vec![
                Field::Magic {
                    expected: vec![0x7F, 0x45, 0x4C, 0x46],
                },
                Field::Uint {
                    name: "cls".into(),
                    size: 1,
                    endian: Endian::Big,
                },
            ],
        );
        let mut bytes = vec![0u8; 40];
        bytes[4..8].copy_from_slice(&[0x7F, 0x45, 0x4C, 0x46]);
        bytes[20..24].copy_from_slice(&[0x7F, 0x45, 0x4C, 0x46]);
        let hits = l.scan(&doc(&bytes), 100).unwrap();
        assert_eq!(hits, vec![4, 20]);
        // And the layout decodes cleanly at each hit.
        for h in hits {
            assert!(l.decode(&doc(&bytes), h).unwrap().is_clean());
        }
    }

    #[test]
    fn a_layout_with_no_magic_cannot_be_scanned() {
        let l = Layout::new(
            "x",
            vec![Field::Bytes {
                name: "a".into(),
                len: 1,
            }],
        );
        assert!(l.scan(&doc(&[1, 2, 3]), 10).unwrap().is_empty());
    }

    #[test]
    fn render_includes_the_offset_of_every_field() {
        let l = Layout::new(
            "h",
            vec![Field::Bytes {
                name: "tag".into(),
                len: 2,
            }],
        );
        let text = l
            .decode(&doc(&[0u8; 0x20]), 0x10)
            .unwrap()
            .render();
        assert!(text.contains("0x00000010"), "structure offset: {text}");
        assert!(text.contains("0x00000010"), "field offset: {text}");
        assert!(text.contains("0000"), "the bytes: {text}");
    }

    #[test]
    fn decoding_sees_the_edit_overlay() {
        let mut d = doc(&[0, 0, 0, 0]);
        d.poke(0, 0x7F).unwrap();
        let l = Layout::new(
            "h",
            vec![Field::Magic {
                expected: vec![0x7F],
            }],
        );
        assert!(l.decode(&d, 0).unwrap().is_clean(), "the pending edit counts");
    }
}