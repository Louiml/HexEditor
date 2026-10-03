//! Undo history.
//!
//! An entry is the byte range that changed plus what was there before, which is
//! enough to restore it exactly because the document keeps the original bytes.
//! Recording the *before* image rather than a diff or an inverse operation means an
//! undo cannot be itself wrong: there is no second piece of logic to get right.
//!
//! Grouping is deliberate and separate from this type. A GUI that wants one undo
//! step per drag rather than per byte pushes a single entry covering the drag's
//! span; [`UndoStack::coalesce_within`] does that by starting a new group when a
//! write lands outside the current one.

use crate::document::Span;

/// One undoable step.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub span: Span,
    /// The bytes in `span` before the write, read out of the document at push time.
    pub before: Vec<u8>,
}

/// A bounded stack of undo steps, most recent last.
#[derive(Debug)]
pub(crate) struct UndoStack {
    entries: Vec<Entry>,
    limit: usize,
}

impl UndoStack {
    pub fn new() -> UndoStack {
        UndoStack {
            entries: Vec::new(),
            limit: 256,
        }
    }

    /// With a history of at most `limit` steps. Zero or less means "keep nothing",
    /// which is a legitimate way to run with undo off.
    pub fn with_limit(limit: usize) -> UndoStack {
        let mut s = UndoStack::new();
        s.limit = limit;
        s
    }

    pub fn can_undo(&self) -> bool {
        !self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Record a write, with the "before" image the caller already read.
    ///
    /// Taking the bytes rather than the document avoids borrowing `Document.undo`
    /// mutably while borrowing the document immutably, which the caller cannot
    /// express.
    pub fn record(&mut self, span: Span, before: Vec<u8>) {
        if self.limit == 0 || span.is_empty() {
            return;
        }
        self.entries.push(Entry { span, before });
        // Drop from the front once over the limit: the oldest step is the least
        // likely to be wanted, and a hex editor session can be long.
        if self.entries.len() > self.limit {
            let excess = self.entries.len() - self.limit;
            self.entries.drain(0..excess);
        }
    }

    pub fn pop(&mut self) -> Option<Entry> {
        self.entries.pop()
    }

    /// True when `span` continues the most recent entry rather than starting a new
    /// step, so a run of adjacent writes collapses into one undo.
    ///
    /// "Continues" means adjacent to or overlapping the last span, within
    /// `tolerance`. Typing forward grows the span one byte at a time and must
    /// coalesce; a write somewhere else in the file must not.
    pub fn coalesce_within(&self, span: &Span, tolerance: u64) -> bool {
        match self.entries.last() {
            Some(last) => {
                span.start <= last.span.end + tolerance && span.end + tolerance >= last.span.start
            }
            None => false,
        }
    }
}

// The GUI drives undo grouping and shows the step count, so `with_limit`, `len`
// and `coalesce_within` are part of the engine's intended surface even though the
// library alone does not call them yet.
#[allow(dead_code)]
fn _surface_is_intended() {
    let _ = UndoStack::with_limit(8);
    let _ = UndoStack::new().len();
    let _ = UndoStack::new().coalesce_within(&Span::new(0, 1), 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_is_enforced_by_dropping_the_oldest() {
        let mut undo = UndoStack::with_limit(3);
        for i in 0..5u64 {
            undo.record(Span::new(i, i + 1), vec![i as u8]);
        }
        assert_eq!(undo.len(), 3);
        // Only the three most recent survive.
        assert!(undo.pop().is_some());
        assert!(undo.pop().is_some());
        assert!(undo.pop().is_some());
        assert!(undo.pop().is_none(), "the two oldest were dropped");
    }

    #[test]
    fn a_zero_limit_records_nothing() {
        let mut undo = UndoStack::with_limit(0);
        undo.record(Span::new(0, 1), vec![1]);
        assert!(!undo.can_undo());
    }

    #[test]
    fn adjacent_growing_spans_coalesce() {
        let mut undo = UndoStack::new();
        undo.record(Span::new(4, 5), vec![4]);
        // Typing the next byte extends the run: one undo step.
        assert!(undo.coalesce_within(&Span::new(5, 6), 0));
        // A byte far away is a separate step.
        assert!(!undo.coalesce_within(&Span::new(40, 41), 0));
        // A write that does not contain the previous span is separate too.
        assert!(!undo.coalesce_within(&Span::new(2, 3), 0));
    }

    #[test]
    fn an_empty_stack_never_coalesces() {
        let undo = UndoStack::new();
        assert!(!undo.coalesce_within(&Span::new(0, 1), 0));
    }
}