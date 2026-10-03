//! Rendering bytes as hex rows.
//!
//! The layout is the familiar one -- offset, hex bytes, ASCII -- and the details
//! that matter are the ones a hex editor gets visibly wrong:
//!
//! * **Grouping.** Sixteen bytes is four groups of four, which is what makes a
//!   column of bytes scannable. The group separator is a space, so a byte column
//!   is greppable with a plain regex.
//! * **The ASCII gutter is fixed width.** A non-printable byte must occupy exactly
//!   one column, or the row jitters and the eye loses its place. Every non-printable
//!   becomes `.`, never a space.
//! * **The offset column is hex, zero-padded to the width the file needs.** A
//!   padded column keeps rows aligned across the whole file, including past 0xFFFF,
//!   where the width naturally grows.

/// Default bytes per row.
pub const DEFAULT_WIDTH: usize = 16;

/// Is this byte worth printing as itself?
///
/// `is_ascii_graphic` excludes the space, so a space byte shows as `.` too --
/// which is right: a hex editor's text column exists to show structure, and a row
/// of spaces is indistinguishable from padding if spaces print as spaces.
pub fn is_text(b: u8) -> bool {
    b.is_ascii_graphic()
}

/// The text-gutter character for a byte.
pub fn text_char(b: u8) -> char {
    if is_text(b) {
        b as char
    } else {
        '.'
    }
}

/// How many hex digits an offset column needs to stay aligned for this file.
pub fn offset_digits(len: u64) -> usize {
    // 0 bytes still needs one digit, so an empty file does not get a zero-width
    // column.
    let bytes = len.max(1);
    let mut digits = 1;
    let mut v = 0x10u64;
    while v <= bytes {
        digits += 1;
        v <<= 4;
    }
    digits.max(4)
}

/// How wide the hex field of a `n`-byte row is.
///
/// One place, because the row body and the padding that keeps the text gutter
/// aligned both need it and must agree exactly. Getting this wrong is what makes a
/// short final row's text column shift left.
fn hex_field_width(n: usize) -> usize {
    // Two digits per byte, one separator between each pair of bytes, and an extra
    // one at each group boundary after the first.
    let groups = (n / 4).saturating_sub(1);
    n * 2 + n.saturating_sub(1) + groups
}

/// Format one row, without a trailing newline.
///
/// `offset` is the row's absolute byte offset, so the offset column stays absolute
/// however the caller is paging.
pub fn format_row(bytes: &[u8], offset: u64, offset_width: usize) -> String {
    let mut out = String::with_capacity(16 + bytes.len() * 4 + 2);
    out.push_str(&format!("{:0width$X}  ", offset, width = offset_width));

    // Hex, grouped in fours so a column of bytes can be scanned vertically.
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(' ');
            // A wider gap every four bytes: sixteen bytes is four groups of four,
            // which is what lets a column be scanned vertically.
            if i % 4 == 0 {
                out.push(' ');
            }
        }
        out.push_str(&format!("{:02X}", b));
    }
    // Pad the hex field to a full row's width so the text gutter lines up on every
    // row, including the short one at the end of a file.
    let target = offset_width + 2 + hex_field_width(DEFAULT_WIDTH);
    while out.len() < target {
        out.push(' ');
    }
    out.push(' ');
    out.push('|');
    for b in bytes {
        out.push(text_char(*b));
    }
    out.push('|');
    out
}

/// Format a whole span as rows, with a trailing newline on each.
pub fn format_span(bytes: &[u8], base_offset: u64, width: usize) -> String {
    let width = width.clamp(1, 256);
    let digits = offset_digits(base_offset + bytes.len() as u64);
    let mut out = String::new();
    for (i, chunk) in bytes.chunks(width).enumerate() {
        out.push_str(&format_row(chunk, base_offset + i as u64 * width as u64, digits));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_row_has_offset_hex_and_text() {
        let bytes: Vec<u8> = (0u8..16).collect();
        let row = format_row(&bytes, 0, 8);
        assert_eq!(row, "00000000  00 01 02 03  04 05 06 07  08 09 0A 0B  0C 0D 0E 0F |................|");
    }

    #[test]
    fn text_bytes_print_as_themselves_and_others_as_a_dot() {
        let bytes = b"Hi\x00\xff\x7f !";
        let row = format_row(bytes, 0, 8);
        assert!(row.ends_with("|Hi....!|"), "got {row:?}");
    }

    #[test]
    fn a_space_byte_is_shown_as_a_dot() {
        // Otherwise a row of spaces is indistinguishable from column padding.
        let row = format_row(b"    ", 0, 8);
        assert!(row.ends_with("|....|"), "got {row:?}");
    }

    #[test]
    fn the_text_gutter_is_the_same_width_for_printable_and_not() {
        let printable = format_row(b"AAAA", 0, 8);
        let binary = format_row(&[0, 1, 2, 3], 0, 8);
        assert_eq!(printable.len(), binary.len());
    }

    #[test]
    fn a_short_last_row_keeps_the_gutter_aligned() {
        let full = format_row(&(0u8..16).collect::<Vec<_>>(), 0, 8);
        let short = format_row(&[0, 1, 2], 0, 8);
        // The `|` that opens the text gutter has to be in the same column on a
        // full row and on a short one.
        let gutter = |row: &str| row.find('|').expect("every row has a gutter");
        assert_eq!(
            gutter(&full),
            gutter(&short),
            "the text gutter must not shift on a short row"
        );
        assert_eq!(full.len(), short.len() + 13, "16 text chars against 3");
    }

    #[test]
    fn the_offset_column_grows_for_a_large_file_and_stays_padded() {
        assert_eq!(offset_digits(0), 4);
        assert_eq!(offset_digits(0xFF), 4);
        assert_eq!(offset_digits(0x1_0000), 5);
        assert_eq!(offset_digits(0x10_0000), 6);
        let row = format_row(&[0], 0x1_0000, offset_digits(0x1_0000));
        assert!(row.starts_with("10000  "), "got {row:?}");
    }

    #[test]
    fn span_formatting_starts_each_row_at_its_own_offset() {
        let bytes: Vec<u8> = (0u8..20).collect();
        let text = format_span(&bytes, 0, 16);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        // The offset column is sized from the span, so 20 bytes needs four digits
        // rather than a fixed eight.
        assert!(lines[0].starts_with("0000  "), "got {:?}", lines[0]);
        assert!(lines[1].starts_with("0010  "), "got {:?}", lines[1]);
    }

    #[test]
    fn a_zero_byte_row_is_not_blank() {
        // The regression this guards: an editor that renders NUL as nothing looks
        // empty, and a file of NULs looks like a zero-byte file.
        let row = format_row(&[0, 0, 0, 0], 0, 8);
        assert!(row.contains("00 00 00 00"), "got {row:?}");
        assert!(row.ends_with("|....|"));
    }
}