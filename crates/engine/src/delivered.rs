//! Remembering which tiles were recently sent back, to skip duplicate requests.

use std::collections::VecDeque;

use crate::{Quality, TileKey, TileRequest};

/// Results sent back, numbered in sending order, remembering which tiles the recent ones
/// carried. A request made before the caller read such a result is a duplicate.
#[derive(Default)]
pub(crate) struct Delivered {
    sent: u64,
    recent: VecDeque<(u64, TileKey, Quality)>,
}

impl Delivered {
    /// How many recent results are remembered; far more than can be in flight at once.
    const REMEMBERED: usize = 1024;

    pub fn record(&mut self, key: TileKey, quality: Quality, rendered: bool) {
        if rendered {
            if self.recent.len() == Self::REMEMBERED {
                self.recent.pop_front();
            }
            self.recent.push_back((self.sent, key, quality));
        }
        self.sent += 1;
    }

    /// Whether `request` asks for a tile sent in a result the caller had not read yet.
    pub fn is_unread(&self, request: &TileRequest, results_read: Option<u64>) -> bool {
        let Some(read) = results_read else {
            return false;
        };
        self.recent
            .iter()
            .rev()
            .take_while(|(n, _, _)| *n >= read)
            .any(|(_, key, quality)| *key == request.key && *quality == request.quality)
    }
}
