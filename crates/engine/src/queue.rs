//! Pending tile requests, ordered by priority.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use crate::{DocId, TileKey, TileRequest};

/// Tile requests waiting to be rendered. Lower `priority` values are rendered first, and
/// requests with equal priority are rendered in the order they arrived.
///
/// Requesting a tile that is already queued replaces the queued request, so the UI can
/// re-request visible tiles every frame without causing duplicate renders.
#[derive(Default)]
pub(crate) struct TileQueue {
    // Heap entries are never removed in place. An entry is stale when `pending` no longer
    // holds the same sequence number for its key; stale entries are skipped when popped.
    heap: BinaryHeap<Reverse<(u32, u64, TileKey)>>,
    pending: HashMap<TileKey, (u64, TileRequest)>,
    next_seq: u64,
}

impl TileQueue {
    pub fn push(&mut self, request: TileRequest) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.pending.insert(request.key, (seq, request));
        self.heap
            .push(Reverse((request.priority, seq, request.key)));
        self.compact_if_needed();
    }

    /// Removes and returns the most urgent request whose generation is at least `min_generation`.
    pub fn pop(&mut self, min_generation: u64) -> Option<TileRequest> {
        while let Some(Reverse((_, seq, key))) = self.heap.pop() {
            match self.pending.get(&key) {
                Some((pending_seq, _)) if *pending_seq == seq => {
                    let (_, request) = self.pending.remove(&key).expect("entry was just found");
                    if request.generation >= min_generation {
                        return Some(request);
                    }
                }
                _ => {} // superseded by a newer request for the same tile
            }
        }
        None
    }

    pub fn drop_older_than(&mut self, generation: u64) {
        self.pending.retain(|_, (_, r)| r.generation >= generation);
        self.compact_if_needed();
    }

    pub fn remove_doc(&mut self, doc: DocId) {
        self.pending.retain(|key, _| key.doc != doc);
        self.compact_if_needed();
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    #[cfg(test)]
    fn heap_len(&self) -> usize {
        self.heap.len()
    }

    fn compact_if_needed(&mut self) {
        if self.heap.len() > 64 && self.heap.len() > self.pending.len() * 4 {
            self.heap = self
                .pending
                .iter()
                .map(|(key, (seq, r))| Reverse((r.priority, *seq, *key)))
                .collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Quality, Scale};

    fn request(tx: u32, priority: u32, generation: u64) -> TileRequest {
        TileRequest {
            key: TileKey {
                doc: DocId(1),
                page: 0,
                scale: Scale::from_px_per_pt(1.0),
                tx,
                ty: 0,
            },
            generation,
            priority,
            quality: Quality::Sharp,
        }
    }

    fn drain(queue: &mut TileQueue, min_generation: u64) -> Vec<u32> {
        std::iter::from_fn(|| queue.pop(min_generation))
            .map(|r| r.key.tx)
            .collect()
    }

    #[test]
    fn most_urgent_first_then_arrival_order() {
        let mut q = TileQueue::default();
        q.push(request(0, 5, 0));
        q.push(request(1, 1, 0));
        q.push(request(2, 5, 0));
        q.push(request(3, 0, 0));
        assert_eq!(drain(&mut q, 0), vec![3, 1, 0, 2]);
        assert!(q.is_empty());
    }

    #[test]
    fn re_requesting_updates_priority_without_duplicates() {
        let mut q = TileQueue::default();
        q.push(request(0, 1, 0));
        q.push(request(1, 2, 0));
        q.push(request(1, 0, 0)); // tile 1 became more urgent
        assert_eq!(drain(&mut q, 0), vec![1, 0]);
    }

    #[test]
    fn stale_generations_are_skipped() {
        let mut q = TileQueue::default();
        q.push(request(0, 0, 1));
        q.push(request(1, 1, 2));
        assert_eq!(drain(&mut q, 2), vec![1]);

        q.push(request(2, 0, 3));
        q.push(request(3, 0, 4));
        q.drop_older_than(4);
        assert_eq!(drain(&mut q, 0), vec![3]);
    }

    #[test]
    fn closing_a_document_drops_its_tiles() {
        let mut q = TileQueue::default();
        q.push(request(0, 0, 0));
        let mut other = request(1, 0, 0);
        other.key.doc = DocId(2);
        q.push(other);
        q.remove_doc(DocId(1));
        assert_eq!(drain(&mut q, 0), vec![1]);
    }

    #[test]
    fn heap_does_not_grow_when_the_same_tiles_are_requested_every_frame() {
        let mut q = TileQueue::default();
        for frame in 0..1000 {
            for tx in 0..8 {
                q.push(request(tx, frame % 3, 0));
            }
        }
        assert!(q.heap_len() <= 65, "heap grew to {}", q.heap_len());
        assert_eq!(drain(&mut q, 0).len(), 8);
    }
}
