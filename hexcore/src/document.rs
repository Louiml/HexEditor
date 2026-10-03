//! The document: the bytes on disk, plus a sparse overlay of pending edits.
//!
//! ## Why an overlay
//!
//! A hex editor's hot loop is "read a row to draw it". If every keystroke wrote
//! through to the file, that loop would be a syscall per row and an editor would
//! feel like treacle; if it kept the whole file in a mutable buffer, opening a
//! 4 GB image would need 4 GB of RAM before the user had looked at anything.
//!
//! So the original bytes are held once and never mutated. Edits go into a sparse
//! overlay -- a sorted map of `offset -> [u8]` -- and a read is "the overlay if
//! there is one, otherwise the original". Only [`Document::save`] touches the file,
//! and it writes the same byte ranges in the same order every time.
//!
//! That also makes undo trivial and exact: an undo entry is the byte range that
//! changed and what was there before, and the original bytes are still there to
//! restore from.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::undo::UndoStack;

/// A byte range half-open as `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}

impl Span {
    pub fn new(start: u64, end: u64) -> Span {
        debug_assert!(start <= end, "span start {start} exceeds end {end}");
        Span { start, end }
    }

    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn contains(&self, offset: u64) -> bool {
        offset >= self.start && offset < self.end
    }
}

/// Why a write was refused.
///
/// Carried as a value rather than a bare `Err<String>` so a GUI can say something
/// useful and a test can assert on the reason. `String` errors lose the structure
/// that makes either possible.
#[derive(Debug)]
pub enum Error {
    /// An offset or length past the end of the document.
    OutOfBounds { offset: u64, len: u64, len_of_file: u64 },
    /// A write to a document opened read-only.
    ReadOnly,
    /// An underlying I/O failure, with the path and the OS error.
    Io { path: PathBuf, source: io::Error },
    /// A structural description that does not match the bytes it was applied to.
    Structure(String),
    /// A pattern that is not a valid search pattern.
    Pattern(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::OutOfBounds { offset, len, len_of_file } => write!(
                f,
                "write of {len} byte(s) at offset {offset} runs past the end of a {len_of_file}-byte file"
            ),
            Error::ReadOnly => write!(f, "the document is open read-only"),
            Error::Io { path, source } => write!(f, "{}: {}", path.display(), source),
            Error::Structure(m) => write!(f, "structure: {m}"),
            Error::Pattern(m) => write!(f, "pattern: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// A file opened for editing: the original bytes, a sparse overlay, and an undo
/// history.
#[derive(Debug)]
pub struct Document {
    path: PathBuf,
    /// The bytes as they were when the file was opened. Never mutated.
    original: Vec<u8>,
    /// Pending edits, keyed by offset. Sparse: an absent offset means "unchanged".
    overlay: BTreeMap<u64, u8>,
    writable: bool,
    /// True once anything has been written to the overlay.
    dirty: bool,
    undo: UndoStack,
}

impl Document {
    /// Open a document, reading the whole file.
    ///
    /// `read_only` refuses writes. The file is read eagerly rather than mapped
    /// because the overlay design already keeps the original bytes in memory, and a
    /// second copy from an OS mapping would double the footprint for no gain. A
    /// future `mmap` backend can satisfy the same read API.
    pub fn open(path: impl AsRef<Path>, read_only: bool) -> Result<Document> {
        let path = path.as_ref().to_path_buf();
        let mut file =
            File::open(&path).map_err(|source| Error::Io { path: path.clone(), source })?;
        let mut original = Vec::new();
        file.read_to_end(&mut original)
            .map_err(|source| Error::Io { path: path.clone(), source })?;
        Ok(Document {
            path,
            original,
            overlay: BTreeMap::new(),
            writable: !read_only,
            dirty: false,
            undo: UndoStack::new(),
        })
    }

    /// An in-memory document, for tests and for the GUI's "new file" case.
    pub fn from_bytes(bytes: Vec<u8>) -> Document {
        Document {
            path: PathBuf::from("<memory>"),
            original: bytes,
            overlay: BTreeMap::new(),
            writable: true,
            dirty: false,
            undo: UndoStack::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The document's length, which the overlay never changes: this editor edits
    /// bytes in place and does not insert or delete.
    pub fn len(&self) -> u64 {
        self.original.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.original.is_empty()
    }

    /// True when the overlay holds something the file does not.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn can_write(&self) -> bool {
        self.writable
    }

    /// How many bytes differ from the file on disk.
    pub fn edited_byte_count(&self) -> u64 {
        self.overlay.len() as u64
    }

    /// The byte at `offset` as it currently reads: the overlay if there is one.
    pub fn byte(&self, offset: u64) -> Result<u8> {
        if offset >= self.len() {
            return Err(Error::OutOfBounds {
                offset,
                len: 1,
                len_of_file: self.len(),
            });
        }
        Ok(self
            .overlay
            .get(&offset)
            .copied()
            .unwrap_or_else(|| self.original[offset as usize]))
    }

    /// A copy of `[start, end)` as it currently reads.
    pub fn slice(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        self.check_range(start, end)?;
        let mut out = Vec::with_capacity((end - start) as usize);
        for off in start..end {
            out.push(self.byte_unchecked(off));
        }
        Ok(out)
    }

    /// Copy `[start, end)` into `out`, avoiding a per-byte bounds check.
    ///
    /// This is the drawing loop's inner function, so it is written to do one map
    /// lookup per contiguous unchanged run rather than one per byte: an untouched
    /// row costs a single lookup, not sixteen.
    pub fn read_into(&self, start: u64, out: &mut [u8]) -> Result<()> {
        let end = start + out.len() as u64;
        self.check_range(start, end)?;
        let mut written = 0usize;
        let mut cursor = start;
        while cursor < end {
            match self.overlay.range(cursor..).next() {
                Some((&off, &byte)) if off < end => {
                    // Everything from the cursor up to the edit is unchanged.
                    let run = (off - cursor) as usize;
                    if run > 0 {
                        out[written..written + run].copy_from_slice(
                            &self.original[cursor as usize..off as usize],
                        );
                        written += run;
                    }
                    out[written] = byte;
                    written += 1;
                    cursor = off + 1;
                }
                // No pending edit at or after the cursor: the rest is original.
                _ => {
                    let run = (end - cursor) as usize;
                    out[written..written + run]
                        .copy_from_slice(&self.original[cursor as usize..end as usize]);
                    written += run;
                    cursor = end;
                }
            }
        }
        Ok(())
    }

    /// Write `bytes` at `offset`, as one undoable step.
    ///
    /// All or nothing: if any part of the range is out of bounds nothing is written,
    /// so a rejected edit cannot leave the document half-modified.
    pub fn write(&mut self, offset: u64, bytes: &[u8]) -> Result<Span> {
        if !self.writable {
            return Err(Error::ReadOnly);
        }
        let end = offset + bytes.len() as u64;
        self.check_range(offset, end)?;

        let mut before = Vec::with_capacity(bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            let off = offset + i as u64;
            before.push(self.byte_unchecked(off));
            // Only record an entry where the value actually changes, so a write
            // that rewrites identical bytes does not mark the document dirty.
            if self.byte_unchecked(off) != *b {
                self.overlay.insert(off, *b);
                self.dirty = true;
            }
        }
        let span = Span::new(offset, end);
        self.undo.record(span, before);
        Ok(span)
    }

    /// Overwrite one byte.
    pub fn poke(&mut self, offset: u64, byte: u8) -> Result<()> {
        self.write(offset, &[byte]).map(|_| ())
    }

    /// Undo the most recent write. False when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some(entry) = self.undo.pop() else {
            return false;
        };
        for (i, b) in entry.before.iter().enumerate() {
            let off = entry.span.start + i as u64;
            // Restore the original byte and drop the overlay entry rather than
            // writing the old value back into the overlay: a byte that matches the
            // file again is not an edit.
            match self.original.get(off as usize) {
                Some(orig) if *orig == *b => {
                    self.overlay.remove(&off);
                }
                _ => {
                    self.overlay.insert(off, *b);
                }
            }
        }
        self.dirty = !self.overlay.is_empty();
        true
    }

    pub fn can_undo(&self) -> bool {
        self.undo.can_undo()
    }

    /// Drop every pending edit and the undo history.
    pub fn revert(&mut self) {
        self.overlay.clear();
        self.dirty = false;
        self.undo.clear();
    }

    /// Write the overlay through to the file.
    ///
    /// Writes only the spans that changed, in ascending offset order, so the file is
    /// touched once per contiguous run rather than once per byte. The overlay is left
    /// in place and the document stays dirty: an edit made after the save is still
    /// unsaved, and conflating the two would lose the user's place.
    pub fn save(&mut self) -> Result<usize> {
        if !self.writable {
            return Err(Error::ReadOnly);
        }
        if self.overlay.is_empty() {
            return Ok(0);
        }
        let mut file = File::options()
            .write(true)
            .open(&self.path)
            .map_err(|source| Error::Io { path: self.path.clone(), source })?;

        let mut written = 0usize;
        for (start, end, bytes) in self.dirty_runs() {
            file.seek(SeekFrom::Start(start))
                .map_err(|source| Error::Io { path: self.path.clone(), source })?;
            file.write_all(&bytes)
                .map_err(|source| Error::Io { path: self.path.clone(), source })?;
            written += bytes.len();
            let _ = end;
        }
        file.flush()
            .map_err(|source| Error::Io { path: self.path.clone(), source })?;
        Ok(written)
    }

    /// The overlay's changed bytes as contiguous `[start, end)` runs.
    ///
    /// Two adjacent changed bytes are one run: a sixteen-byte row that the user
    /// overwrote in full is one seek and one write, not sixteen.
    pub fn dirty_runs(&self) -> Vec<(u64, u64, Vec<u8>)> {
        let mut runs: Vec<(u64, u64, Vec<u8>)> = Vec::new();
        for (&off, &b) in &self.overlay {
            match runs.last_mut() {
                Some(last) if last.1 == off => {
                    last.1 = off + 1;
                    last.2.push(b);
                }
                _ => runs.push((off, off + 1, vec![b])),
            }
        }
        runs
    }

    /// Every offset the overlay has touched, ascending. Used by tests and by the
    /// GUI's "unsaved changes" marker.
    pub fn edited_offsets(&self) -> Vec<u64> {
        self.overlay.keys().copied().collect()
    }

    fn byte_unchecked(&self, offset: u64) -> u8 {
        self.overlay
            .get(&offset)
            .copied()
            .unwrap_or(self.original[offset as usize])
    }

    fn check_range(&self, start: u64, end: u64) -> Result<()> {
        if end > self.len() {
            return Err(Error::OutOfBounds {
                offset: start,
                len: end - start,
                len_of_file: self.len(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Document {
        Document::from_bytes((0u8..32).collect())
    }

    #[test]
    fn a_fresh_document_reads_the_original_bytes() {
        let d = doc();
        assert_eq!(d.len(), 32);
        assert!(!d.is_dirty());
        assert_eq!(d.byte(0).unwrap(), 0);
        assert_eq!(d.byte(31).unwrap(), 31);
    }

    #[test]
    fn a_write_is_visible_to_reads_and_marks_the_document_dirty() {
        let mut d = doc();
        d.poke(4, 0xFF).unwrap();
        assert_eq!(d.byte(4).unwrap(), 0xFF);
        assert!(d.is_dirty());
        assert_eq!(d.edited_byte_count(), 1);
        // The original is untouched, which is what makes undo exact.
        assert_eq!(d.original[4], 4);
    }

    #[test]
    fn rewriting_identical_bytes_does_not_make_the_document_dirty() {
        let mut d = doc();
        d.poke(4, 4).unwrap();
        assert!(!d.is_dirty(), "writing the value already there is not an edit");
        assert_eq!(d.edited_byte_count(), 0);
    }

    #[test]
    fn a_read_only_document_refuses_writes() {
        let mut d = doc();
        d.writable = false;
        assert!(matches!(d.poke(0, 1), Err(Error::ReadOnly)));
        assert!(matches!(d.write(0, &[1, 2]), Err(Error::ReadOnly)));
        assert!(!d.is_dirty(), "a refused write must leave nothing behind");
    }

    #[test]
    fn an_out_of_bounds_write_is_refused_whole() {
        let mut d = doc();
        let err = d.write(30, &[1, 2, 3, 4]).unwrap_err();
        match err {
            Error::OutOfBounds { offset, len, len_of_file } => {
                assert_eq!((offset, len, len_of_file), (30, 4, 32));
            }
            other => panic!("expected OutOfBounds, got {other:?}"),
        }
        // Nothing was applied, so the document is untouched rather than half-edited.
        assert!(!d.is_dirty());
        assert_eq!(d.byte(30).unwrap(), 30);
    }

    #[test]
    fn undo_restores_the_previous_bytes_exactly() {
        let mut d = doc();
        d.write(8, &[0xAA, 0xBB]).unwrap();
        assert_eq!(d.slice(8, 10).unwrap(), vec![0xAA, 0xBB]);
        assert!(d.undo());
        assert_eq!(d.slice(8, 10).unwrap(), vec![8, 9]);
        assert!(!d.is_dirty(), "undoing the only edit leaves it clean");
        assert!(!d.undo(), "nothing left to undo");
    }

    #[test]
    fn undo_across_several_edits_unwinds_in_order() {
        let mut d = doc();
        d.poke(0, 0x11).unwrap();
        d.poke(1, 0x22).unwrap();
        d.poke(2, 0x33).unwrap();
        assert_eq!(d.slice(0, 3).unwrap(), vec![0x11, 0x22, 0x33]);
        assert!(d.undo());
        assert_eq!(d.slice(0, 3).unwrap(), vec![0x11, 0x22, 2]);
        assert!(d.undo());
        assert_eq!(d.slice(0, 3).unwrap(), vec![0x11, 1, 2]);
        assert!(d.undo());
        assert_eq!(d.slice(0, 3).unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn an_undone_byte_returns_to_matching_the_file() {
        let mut d = doc();
        d.poke(5, 0x99).unwrap();
        d.undo();
        // The overlay entry is gone, not present-and-equal, so the byte is no
        // longer counted as an edit.
        assert!(d.edited_offsets().is_empty());
    }

    #[test]
    fn adjacent_edits_collapse_into_one_run() {
        let mut d = doc();
        d.write(0, &[1, 2, 3]).unwrap();
        d.write(3, &[4]).unwrap();
        let runs = d.dirty_runs();
        assert_eq!(runs.len(), 1, "0..4 is contiguous: {runs:?}");
        assert_eq!(runs[0].0, 0);
        assert_eq!(runs[0].1, 4);
        assert_eq!(runs[0].2, vec![1, 2, 3, 4]);
    }

    #[test]
    fn separated_edits_stay_separate_runs() {
        let mut d = doc();
        d.poke(0, 1).unwrap();
        d.poke(10, 2).unwrap();
        let runs = d.dirty_runs();
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].0, runs[0].1), (0, 1));
        assert_eq!((runs[1].0, runs[1].1), (10, 11));
    }

    #[test]
    fn read_into_matches_byte_by_byte_reads() {
        let mut d = doc();
        d.poke(3, 0xAA).unwrap();
        d.poke(4, 0xBB).unwrap();
        d.poke(20, 0xCC).unwrap();
        for start in 0..32u64 {
            let end = (start + 5).min(32);
            let mut buf = vec![0u8; (end - start) as usize];
            d.read_into(start, &mut buf).unwrap();
            let mut slow = Vec::new();
            for off in start..end {
                slow.push(d.byte(off).unwrap());
            }
            assert_eq!(buf, slow, "mismatch reading {start}..{end}");
        }
    }

    #[test]
    fn slice_out_of_bounds_is_refused() {
        let d = doc();
        assert!(matches!(
            d.slice(30, 40),
            Err(Error::OutOfBounds { .. })
        ));
        assert_eq!(d.slice(32, 32).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn revert_drops_every_edit() {
        let mut d = doc();
        d.write(0, &[9, 9, 9]).unwrap();
        d.revert();
        assert!(!d.is_dirty());
        assert_eq!(d.slice(0, 3).unwrap(), vec![0, 1, 2]);
        assert!(!d.can_undo());
    }
}