// Snapshot undo/redo with bounded depth and saved-revision tracking.
//
// Snapshots are whole documents. Keep large immutable payloads behind `Arc`
// in the document type so a checkpoint copies handles, not bytes.

/// Undo depth. Documents that hold editable geometry are large even when
/// payloads are shared; 32 steps has been enough for terrain sculpting.
pub const DEPTH: usize = 32;

struct Snapshot<T> {
    document: T,
    revision: u64,
}

/// Owns the undo and redo stacks for one document of type `T`.
///
/// Every checkpoint gets a fresh revision. A document is dirty when its
/// current revision differs from the last one marked saved, so undoing back
/// to the saved state clears the dirty flag.
pub struct History<T> {
    past: Vec<Snapshot<T>>,
    future: Vec<Snapshot<T>>,
    revision: u64,
    next_revision: u64,
    saved: Option<u64>,
}

impl<T: Clone> History<T> {
    /// `saved` says whether the initial document matches a file on disk.
    pub fn new(saved: bool) -> Self {
        Self {
            past: vec![],
            future: vec![],
            revision: 0,
            next_revision: 1,
            saved: saved.then_some(0),
        }
    }

    /// Records `document` as it is before an edit.
    pub fn checkpoint(&mut self, document: &T) {
        self.checkpoint_owned(document.clone());
    }

    /// Records a snapshot taken earlier, such as the start of a drag gesture.
    pub fn checkpoint_owned(&mut self, document: T) {
        if self.past.len() == DEPTH {
            self.past.remove(0);
        }
        self.past.push(Snapshot {
            document,
            revision: self.revision,
        });
        self.future.clear();
        self.revision = self.next_revision;
        self.next_revision += 1;
    }

    pub fn undo(&mut self, document: &mut T) -> bool {
        let Some(old) = self.past.pop() else {
            return false;
        };
        self.future.push(Snapshot {
            document: std::mem::replace(document, old.document),
            revision: self.revision,
        });
        self.revision = old.revision;
        true
    }

    pub fn redo(&mut self, document: &mut T) -> bool {
        let Some(new) = self.future.pop() else {
            return false;
        };
        self.past.push(Snapshot {
            document: std::mem::replace(document, new.document),
            revision: self.revision,
        });
        self.revision = new.revision;
        true
    }

    pub fn mark_saved(&mut self) {
        self.saved = Some(self.revision);
    }

    pub fn dirty(&self) -> bool {
        self.saved != Some(self.revision)
    }

    /// Changes on every edit, undo and redo. Use it as a cache key for derived data.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    /// Read-only retained documents, for memory diagnostics and history previews.
    /// Includes both undo and redo snapshots; does not include the live document.
    pub fn snapshots(&self) -> impl Iterator<Item = &T> {
        self.past
            .iter()
            .chain(&self.future)
            .map(|snapshot| &snapshot.document)
    }

    /// The undo snapshots, the next to undo to first, for hosts that store
    /// a snapshot as a delta against its neighbour. Undo hands them back in
    /// this order, so a snapshot's newer neighbour is the one before it
    /// here, or the live document.
    pub fn past_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.past
            .iter_mut()
            .rev()
            .map(|snapshot| &mut snapshot.document)
    }

    /// The redo snapshots, the next to redo to first.
    pub fn future_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.future
            .iter_mut()
            .rev()
            .map(|snapshot| &mut snapshot.document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_come_back_newest_first() {
        let mut doc = 0;
        let mut history = History::new(true);
        for v in 1..=3 {
            history.checkpoint(&doc);
            doc = v;
        }
        assert_eq!(
            history.past_mut().map(|d| *d).collect::<Vec<_>>(),
            [2, 1, 0]
        );
        // Hosts may rewrite what a snapshot holds; undo returns it.
        *history.past_mut().next().unwrap() = 20;
        history.undo(&mut doc);
        assert_eq!(doc, 20);
        assert_eq!(history.future_mut().map(|d| *d).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn undo_to_saved_state_is_clean_and_new_edits_drop_redo() {
        let mut doc = 1;
        let mut history = History::new(true);
        assert!(!history.dirty());
        history.checkpoint(&doc);
        doc = 2;
        assert!(history.dirty());
        assert!(history.undo(&mut doc));
        assert_eq!(doc, 1);
        assert!(!history.dirty());
        assert!(history.redo(&mut doc));
        assert_eq!(doc, 2);
        history.mark_saved();
        history.undo(&mut doc);
        history.checkpoint(&doc);
        doc = 3;
        assert!(!history.can_redo());
        assert!(history.dirty());
        assert!(!history.redo(&mut doc));
    }

    #[test]
    fn depth_is_bounded_and_revisions_stay_unique() {
        let mut doc = 0;
        let mut history = History::new(false);
        let mut seen = std::collections::BTreeSet::from([history.revision()]);
        for i in 1..=DEPTH + 10 {
            history.checkpoint(&doc);
            doc = i;
            assert!(seen.insert(history.revision()));
        }
        let mut undone = 0;
        while history.undo(&mut doc) {
            undone += 1;
        }
        assert_eq!(undone, DEPTH);
        assert_eq!(doc, 10);
        assert!(history.dirty());
    }
}
