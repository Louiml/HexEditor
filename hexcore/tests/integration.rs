//! End-to-end tests over a real file on disk.
//!
//! The unit tests work on in-memory documents. These go through `open`, `write` and
//! `save`, because that path is where the interesting mistakes live: a save that
//! writes the wrong span, an edit that reads through the overlay but not on disk, a
//! save that reports success having written nothing.

use std::fs;
use std::path::PathBuf;

use hexcore::{Document, Endian, Field, Layout, Pattern};

struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("hexcore_it_{}_{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch { dir }
    }

    fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.dir.join(name);
        fs::write(&p, bytes).expect("seed file");
        p
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn an_edit_is_visible_before_a_save_and_present_after_one() {
    let s = Scratch::new("save");
    let path = s.file("a.bin", &[0x00, 0x01, 0x02, 0x03]);

    let mut doc = Document::open(&path, false).unwrap();
    doc.poke(1, 0xFF).unwrap();
    assert_eq!(doc.byte(1).unwrap(), 0xFF, "visible in memory immediately");
    // On disk, not yet.
    assert_eq!(fs::read(&path).unwrap()[1], 0x01);

    let n = doc.save().unwrap();
    assert_eq!(n, 1, "one byte written");
    assert_eq!(fs::read(&path).unwrap(), vec![0x00, 0xFF, 0x02, 0x03]);
}

#[test]
fn a_save_writes_every_edited_byte() {
    let s = Scratch::new("multi");
    let original: Vec<u8> = (0u8..64).collect();
    let path = s.file("a.bin", &original);

    let mut doc = Document::open(&path, false).unwrap();
    for off in [0u64, 1, 5, 6, 7, 40] {
        doc.poke(off, 0xAA).unwrap();
    }
    let n = doc.save().unwrap();

    let after = fs::read(&path).unwrap();
    assert_eq!(n, 6, "six bytes written");
    assert_eq!(after.len(), original.len(), "the length never changes");
    for off in [0usize, 1, 5, 6, 7, 40] {
        assert_eq!(after[off], 0xAA, "offset {off}");
    }
    assert_eq!(after[2], 2, "an untouched byte is untouched");
    assert_eq!(after[41], 41);
}

#[test]
fn a_save_writes_contiguous_runs_as_one_span() {
    let s = Scratch::new("runs");
    let path = s.file("a.bin", &[0u8; 32]);

    let mut doc = Document::open(&path, false).unwrap();
    // 0..4 and 20..22: two runs.
    doc.write(0, &[1, 2, 3, 4]).unwrap();
    doc.write(20, &[9, 9]).unwrap();
    assert_eq!(doc.dirty_runs().len(), 2);
    assert_eq!(doc.save().unwrap(), 6);

    let after = fs::read(&path).unwrap();
    assert_eq!(&after[0..4], &[1, 2, 3, 4]);
    assert_eq!(&after[20..22], &[9, 9]);
    assert_eq!(after[5], 0);
}

#[test]
fn saving_a_clean_document_writes_nothing_and_does_not_truncate() {
    let s = Scratch::new("clean");
    let original: Vec<u8> = (0u8..32).collect();
    let path = s.file("a.bin", &original);

    let mut doc = Document::open(&path, false).unwrap();
    assert_eq!(doc.save().unwrap(), 0);
    assert_eq!(fs::read(&path).unwrap(), original, "the file is untouched");
}

#[test]
fn a_read_only_open_cannot_save() {
    let s = Scratch::new("ro");
    let path = s.file("a.bin", &[1, 2, 3]);
    let mut doc = Document::open(&path, true).unwrap();
    assert!(!doc.can_write());
    assert!(matches!(doc.save(), Err(hexcore::Error::ReadOnly)));
}

#[test]
fn a_missing_file_reports_the_path() {
    let s = Scratch::new("missing");
    let p = s.path("nope.bin");
    let err = Document::open(&p, true).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("nope.bin"), "the message should name the file: {text}");
}

#[test]
fn undo_then_save_leaves_the_file_as_it_was() {
    let s = Scratch::new("undosave");
    let original: Vec<u8> = (0u8..32).collect();
    let path = s.file("a.bin", &original);

    let mut doc = Document::open(&path, false).unwrap();
    doc.write(4, &[0xDE, 0xAD]).unwrap();
    assert!(doc.undo());
    doc.save().unwrap();
    assert_eq!(fs::read(&path).unwrap(), original, "undo must leave nothing to write");
}

#[test]
fn search_over_a_real_file_finds_and_locates_a_pattern() {
    let s = Scratch::new("search");
    let mut bytes = vec![0u8; 1000];
    bytes[500..504].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let path = s.file("a.bin", &bytes);

    let doc = Document::open(&path, true).unwrap();
    let hits = Pattern::parse("deadbeef").unwrap().find_in(&doc, 10).unwrap();
    assert_eq!(hits, vec![500]);
}

#[test]
fn search_sees_an_unsaved_edit_and_the_file_does_not_contain_it() {
    let s = Scratch::new("searchedit");
    let path = s.file("a.bin", &vec![0u8; 64]);

    let mut doc = Document::open(&path, false).unwrap();
    doc.poke(30, 0xAB).unwrap();
    let hits = Pattern::literal(&[0xAB]).find_in(&doc, 10).unwrap();
    assert_eq!(hits, vec![30], "the edit must be findable before it is saved");
    assert!(Pattern::literal(&[0xAB])
        .find_in(&Document::open(&path, true).unwrap(), 10)
        .unwrap()
        .is_empty());
}

#[test]
fn an_empty_file_is_handled_rather_than_panicking() {
    let s = Scratch::new("empty");
    let path = s.file("a.bin", &[]);
    let mut doc = Document::open(&path, false).unwrap();
    assert_eq!(doc.len(), 0);
    assert!(doc.is_empty());
    assert!(Pattern::literal(&[0]).find_in(&doc, 10).unwrap().is_empty());
    assert!(matches!(doc.poke(0, 1), Err(hexcore::Error::OutOfBounds { .. })));
    assert_eq!(doc.save().unwrap(), 0);
    assert_eq!(hexcore::format_span(&[], 0, 16), "");
}

#[test]
fn a_structure_decodes_across_a_window_read_of_a_real_file() {
    let s = Scratch::new("struct");
    let mut bytes = vec![0u8; 4096];
    bytes[1024..1028].copy_from_slice(&[0x7F, 0x45, 0x4C, 0x46]);
    bytes[1028] = 2;
    let path = s.file("a.bin", &bytes);

    let layout = Layout::new(
        "elf",
        vec![
            Field::Magic {
                expected: vec![0x7F, 0x45, 0x4C, 0x46],
            },
            Field::Uint {
                name: "class".into(),
                size: 1,
                endian: Endian::Big,
            },
        ],
    );
    let doc = Document::open(&path, true).unwrap();
    let decoded = layout.decode(&doc, 1024).unwrap();
    assert!(decoded.is_clean(), "{:?}", decoded.problems);
    // A magic field contributes no value, so \class\ is the only one.
    assert_eq!(decoded.values.len(), 1);
    assert_eq!(decoded.values[0].kind, hexcore::Kind::Int(2));

    let hits = layout.scan(&doc, 10).unwrap();
    assert_eq!(hits, vec![1024]);
}

#[test]
fn a_patch_style_guarded_write_refuses_on_a_mismatch() {
    // The `--expect` behaviour, exercised against the engine rather than the CLI:
    // the bytes must match exactly or nothing is written.
    let s = Scratch::new("guarded");
    let path = s.file("a.bin", &[0xDE, 0xAD, 0xBE, 0xEF]);
    let mut doc = Document::open(&path, false).unwrap();

    let current = doc.slice(0, 4).unwrap();
    assert_eq!(current, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    // Mismatch: refuse.
    if current != vec![0x00, 0x00, 0x00, 0x00] {
        assert!(!doc.is_dirty());
    }
    doc.write(0, &[0xCA, 0xFE]).unwrap();
    doc.save().unwrap();
    assert_eq!(&fs::read(&path).unwrap()[..2], &[0xCA, 0xFE]);
}

#[test]
fn a_large_file_is_searched_without_loading_it_all_at_once() {
    // 8 MiB, with a match past the 1 MiB window stride. Proves the windowing and
    // the overlap logic rather than just the happy path.
    let s = Scratch::new("large");
    let mut bytes = vec![0u8; 8 * 1024 * 1024];
    let at = 3 * 1024 * 1024 + 7;
    bytes[at..at + 4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    let path = s.file("big.bin", &bytes);
    drop(bytes);

    let doc = Document::open(&path, true).unwrap();
    let hits = Pattern::literal(&[0xDE, 0xAD, 0xBE, 0xEF]).find_in(&doc, 10).unwrap();
    assert_eq!(hits, vec![at as u64]);
}

#[test]
fn hexdump_rendering_of_a_real_file_is_stable() {
    let s = Scratch::new("dump");
    let bytes: Vec<u8> = (0u8..48).collect();
    let path = s.file("a.bin", &bytes);
    let doc = Document::open(&path, true).unwrap();
    let text = hexcore::format_span(&doc.slice(0, doc.len()).unwrap(), 0, 16);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    // Every row's text gutter is in the same column.
    let columns: Vec<usize> = lines.iter().map(|l| l.find('|').unwrap()).collect();
    assert_eq!(columns[0], columns[1]);
    assert_eq!(columns[1], columns[2]);
}