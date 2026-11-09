// Background work bookkeeping: single-flight jobs where only the latest
// request matters (streaming the region under a moving camera), and keyed
// values loaded off the UI thread a batch at a time (models, textures).
use gpui::Task;
use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Owns at most one running job keyed by `K`, with optional progress `P`.
///
/// A new key cancels the running job but does not start until that job has
/// called [`LatestTask::finish`]. Synchronous file reads outlive a dropped
/// GPUI task handle, so two workers would otherwise compete for the disk.
/// Call `begin` again after `finish` to admit whatever is newest by then;
/// intermediate keys are skipped.
pub struct LatestTask<K, P = ()> {
    task: Option<Task<()>>,
    token: Arc<AtomicUsize>,
    requested: Option<K>,
    pub progress: Option<P>,
}

/// Handed to the worker. Check it between units of work and stop early.
#[derive(Clone, Debug)]
pub struct Ticket {
    id: usize,
    token: Arc<AtomicUsize>,
}

impl Ticket {
    pub fn canceled(&self) -> bool {
        self.token.load(Ordering::Relaxed) != self.id
    }
}

impl<K: PartialEq, P> Default for LatestTask<K, P> {
    fn default() -> Self {
        Self {
            task: None,
            token: Arc::new(AtomicUsize::new(0)),
            requested: None,
            progress: None,
        }
    }
}

impl<K: PartialEq, P> LatestTask<K, P> {
    /// Returns a ticket when the caller should start work for `key` now.
    /// `None` means `key` is already loaded or running, or that an older job
    /// is still winding down.
    pub fn begin(&mut self, key: K) -> Option<Ticket> {
        if self.requested.as_ref() == Some(&key) {
            return None;
        }
        if self.task.is_some() {
            if self.requested.is_some() {
                self.cancel();
            }
            return None;
        }
        self.cancel();
        self.requested = Some(key);
        Some(Ticket {
            id: self.token.load(Ordering::Relaxed),
            token: self.token.clone(),
        })
    }

    /// Keeps the job alive. Dropping a GPUI task cancels its future.
    pub fn run(&mut self, task: Task<()>) {
        self.task = Some(task);
    }

    /// Invalidates the running ticket and forgets the requested key.
    pub fn cancel(&mut self) {
        self.token.fetch_add(1, Ordering::Relaxed);
        self.requested = None;
        self.progress = None;
    }

    /// Call from the job's completion. Returns whether its result is still wanted.
    pub fn finish(&mut self, ticket: &Ticket) -> bool {
        self.task = None;
        self.progress = None;
        !ticket.canceled()
    }

    pub fn running(&self) -> bool {
        self.task.is_some()
    }
}

impl<K, P> Drop for LatestTask<K, P> {
    fn drop(&mut self) {
        self.token.fetch_add(1, Ordering::Relaxed);
    }
}

/// Values loaded by key in background batches: what is loaded, queued or
/// failed. The host asks for the next batch of keys it wants, loads them on
/// a worker, and hands the results back; nothing is requested twice.
pub struct Batches<K, V> {
    pub loaded: HashMap<K, V>,
    queued: HashSet<K>,
    failed: usize,
}

impl<K, V> Default for Batches<K, V> {
    fn default() -> Self {
        Self {
            loaded: HashMap::new(),
            queued: HashSet::new(),
            failed: 0,
        }
    }
}

impl<K: Hash + Eq + Clone, V> Batches<K, V> {
    /// Up to `max` keys from `wanted` that are neither loaded nor queued;
    /// they are queued now. Duplicates in `wanted` are skipped.
    pub fn next_batch<'a, Q>(
        &mut self,
        wanted: impl IntoIterator<Item = &'a Q>,
        max: usize,
    ) -> Vec<K>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ToOwned<Owned = K> + ?Sized + 'a,
    {
        let mut batch = Vec::new();
        for key in wanted {
            if batch.len() == max {
                break;
            }
            if !self.loaded.contains_key(key) && !self.queued.contains(key) {
                self.queued.insert(key.to_owned());
                batch.push(key.to_owned());
            }
        }
        batch
    }

    /// Records a batch's results. `None` is a failure; failed keys stay
    /// queued so they are not retried on every refresh.
    pub fn finish(&mut self, results: impl IntoIterator<Item = (K, Option<V>)>) {
        for (key, value) in results {
            match value {
                Some(v) => {
                    self.queued.remove(&key);
                    self.loaded.insert(key, v);
                }
                None => self.failed += 1,
            }
        }
    }

    pub fn failed(&self) -> usize {
        self.failed
    }

    /// Keys queued and still loading.
    pub fn busy(&self) -> usize {
        self.queued.len().saturating_sub(self.failed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_skip_loaded_queued_and_failed_keys() {
        let mut models = Batches::<String, u32>::default();
        let wanted = ["a", "b", "a", "c"];
        assert_eq!(models.next_batch(wanted, 2), ["a", "b"]);
        assert_eq!(models.next_batch(wanted, 2), ["c"]);
        assert!(models.next_batch(wanted, 2).is_empty());
        assert_eq!(models.busy(), 3);
        models.finish([("a".to_owned(), Some(1)), ("b".to_owned(), None)]);
        assert_eq!(models.loaded.get("a"), Some(&1));
        assert_eq!((models.failed(), models.busy()), (1, 1));
        assert!(models.next_batch(wanted, 4).is_empty());
    }

    #[test]
    fn new_keys_wait_for_the_running_job_and_skip_intermediate_requests() {
        let mut stream = LatestTask::<i32>::default();
        let first = stream.begin(1).unwrap();
        stream.run(Task::ready(()));
        assert!(stream.begin(1).is_none());
        for region in 2..10 {
            assert!(stream.begin(region).is_none());
            assert!(first.canceled());
        }
        assert!(!stream.finish(&first));
        let latest = stream.begin(100).unwrap();
        stream.run(Task::ready(()));
        assert!(stream.finish(&latest));
        assert!(stream.begin(100).is_none());
    }

    #[test]
    fn dropping_the_owner_cancels_outstanding_tickets() {
        let mut stream = LatestTask::<i32>::default();
        let ticket = stream.begin(1).unwrap();
        assert!(!ticket.canceled());
        drop(stream);
        assert!(ticket.canceled());
    }
}
