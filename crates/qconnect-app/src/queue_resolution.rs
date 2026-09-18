//! Queue cursor abstractions and index/order resolution helpers used to
//! map between cloud-side queue/renderer state and local queue indices.
//!
//! The QConnect protocol mixes queue_item_id, track_id, and ordering
//! data across multiple separate frames; this module owns the lookups
//! that reconcile those signals into a single coherent cursor.

use crate::QConnectQueueState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QconnectOrderedQueueCursor {
    Queue(usize),
    Autoplay(usize),
}

pub fn is_valid_ordered_queue_shuffle_order(order: &[usize], track_count: usize) -> bool {
    if order.len() != track_count {
        return false;
    }
    let mut seen = vec![false; track_count];
    for &index in order {
        if index >= track_count || seen[index] {
            return false;
        }
        seen[index] = true;
    }
    true
}

pub fn ordered_queue_cursors(queue: &QConnectQueueState) -> Vec<QconnectOrderedQueueCursor> {
    let mut cursors = if queue.shuffle_mode {
        queue
            .shuffle_order
            .as_ref()
            .filter(|order| is_valid_ordered_queue_shuffle_order(order, queue.queue_items.len()))
            .map(|order| {
                order
                    .iter()
                    .copied()
                    .map(QconnectOrderedQueueCursor::Queue)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                queue
                    .queue_items
                    .iter()
                    .enumerate()
                    .map(|(index, _)| QconnectOrderedQueueCursor::Queue(index))
                    .collect::<Vec<_>>()
            })
    } else {
        queue
            .queue_items
            .iter()
            .enumerate()
            .map(|(index, _)| QconnectOrderedQueueCursor::Queue(index))
            .collect::<Vec<_>>()
    };

    cursors.extend(
        queue
            .autoplay_items
            .iter()
            .enumerate()
            .map(|(index, _)| QconnectOrderedQueueCursor::Autoplay(index)),
    );
    cursors
}

pub fn queue_item_track_id_for_cursor(
    queue: &QConnectQueueState,
    cursor: QconnectOrderedQueueCursor,
) -> Option<u64> {
    match cursor {
        QconnectOrderedQueueCursor::Queue(index) => {
            queue.queue_items.get(index).map(|item| item.track_id)
        }
        QconnectOrderedQueueCursor::Autoplay(index) => {
            queue.autoplay_items.get(index).map(|item| item.track_id)
        }
    }
}

pub fn normalized_queue_item_id_for_cursor(
    queue: &QConnectQueueState,
    cursor: QconnectOrderedQueueCursor,
) -> Option<u64> {
    match cursor {
        QconnectOrderedQueueCursor::Queue(index) => Some(
            normalize_current_queue_item_id_from_queue_state(queue, index),
        ),
        QconnectOrderedQueueCursor::Autoplay(index) => queue
            .autoplay_items
            .get(index)
            .map(|item| item.queue_item_id),
    }
}

pub fn find_cursor_index_by_queue_item_id(
    cursors: &[QconnectOrderedQueueCursor],
    queue: &QConnectQueueState,
    queue_item_id: Option<u64>,
) -> Option<usize> {
    let queue_item_id = queue_item_id?;
    cursors.iter().position(|cursor| {
        normalized_queue_item_id_for_cursor(queue, *cursor) == Some(queue_item_id)
            || match cursor {
                QconnectOrderedQueueCursor::Queue(index) => queue
                    .queue_items
                    .get(*index)
                    .map(|item| item.queue_item_id == queue_item_id)
                    .unwrap_or(false),
                QconnectOrderedQueueCursor::Autoplay(index) => queue
                    .autoplay_items
                    .get(*index)
                    .map(|item| item.queue_item_id == queue_item_id)
                    .unwrap_or(false),
            }
    })
}

pub fn resolve_queue_item_ids_from_queue_state(
    queue: &QConnectQueueState,
    track_id: u64,
) -> (Option<u64>, Option<u64>, Option<u64>) {
    if let Some(current_index) = queue
        .queue_items
        .iter()
        .position(|item| item.track_id == track_id)
    {
        let current_qid = normalize_current_queue_item_id_from_queue_state(queue, current_index);
        let next_item = if queue.shuffle_mode {
            queue
                .shuffle_order
                .as_ref()
                .and_then(|order| {
                    order
                        .iter()
                        .position(|queue_index| *queue_index == current_index)
                        .and_then(|order_index| order.get(order_index + 1))
                        .and_then(|queue_index| queue.queue_items.get(*queue_index))
                })
                .or_else(|| queue.queue_items.get(current_index + 1))
                .or_else(|| queue.autoplay_items.first())
        } else {
            queue
                .queue_items
                .get(current_index + 1)
                .or_else(|| queue.autoplay_items.first())
        };

        return (
            Some(current_qid),
            next_item.map(|item| item.queue_item_id),
            next_item.map(|item| item.track_id),
        );
    }

    if let Some(current_index) = queue
        .autoplay_items
        .iter()
        .position(|item| item.track_id == track_id)
    {
        let current_item = &queue.autoplay_items[current_index];
        let next_item = queue.autoplay_items.get(current_index + 1);
        return (
            Some(current_item.queue_item_id),
            next_item.map(|item| item.queue_item_id),
            next_item.map(|item| item.track_id),
        );
    }

    (None, None, None)
}

pub fn dedupe_track_ids(queue_state: &QConnectQueueState) -> Vec<u64> {
    let mut unique = Vec::with_capacity(queue_state.queue_items.len());
    for item in &queue_state.queue_items {
        if !unique.contains(&item.track_id) {
            unique.push(item.track_id);
        }
    }
    unique
}

pub fn resolve_remote_start_index(
    queue_state: &QConnectQueueState,
    renderer_queue_item_id: Option<u64>,
    renderer_track_id: Option<u64>,
) -> Option<usize> {
    if let Some(queue_item_id) = renderer_queue_item_id {
        // An exact id first, then the NORMALIZED head: the cloud names a
        // freshly-pushed queue's placeholder head by the 0 the renderer reports
        // for it, which no raw id in the queue carries. 0 is also an ordinary id
        // further down a queue, so the exact match has to win — otherwise naming
        // that item lands the cursor on the head instead.
        let by_queue_item_id = queue_state
            .queue_items
            .iter()
            .position(|item| item.queue_item_id == queue_item_id)
            .or_else(|| {
                (0..queue_state.queue_items.len()).find(|&index| {
                    normalize_current_queue_item_id_from_queue_state(queue_state, index)
                        == queue_item_id
                })
            });
        if let Some(index) = by_queue_item_id {
            // Only trust the queue_item_id when the track at that position matches
            // the renderer's reported track (or no track was reported). A qid that
            // resolves to a DIFFERENT track means the cached renderer projection is
            // STALE relative to THIS queue — e.g. a fresh album was just pushed (new
            // track_context_uuid, autoplay_reset) while the projection still names
            // the PREVIOUS queue's item. Trusting the stale qid lands the cursor on
            // the wrong track (the "NowPlayingBar shows track 4 on a freshly-pushed
            // album" bug, controlling a peer that was already rendering). Fall
            // through to the track_id lookup; it won't find the old track in the new
            // queue, so the caller defaults to the queue head.
            let track_matches = renderer_track_id
                .map(|track_id| queue_state.queue_items[index].track_id == track_id)
                .unwrap_or(true);
            if track_matches {
                return Some(index);
            }
        }
    }

    if let Some(track_id) = renderer_track_id {
        if let Some(index) = queue_state
            .queue_items
            .iter()
            .position(|item| item.track_id == track_id)
        {
            return Some(index);
        }
    }

    None
}

pub fn resolve_core_shuffle_order(
    queue_state: &QConnectQueueState,
    renderer_queue_item_id: Option<u64>,
    renderer_track_id: Option<u64>,
    renderer_next_queue_item_id: Option<u64>,
    renderer_next_track_id: Option<u64>,
) -> Option<Vec<usize>> {
    if !queue_state.shuffle_mode {
        return None;
    }

    let raw_order = queue_state
        .shuffle_order
        .as_ref()
        .filter(|order| is_valid_ordered_queue_shuffle_order(order, queue_state.queue_items.len()));

    if raw_order.is_none() {
        log::debug!(
            "[QConnect] resolve_core_shuffle_order: raw_order invalid or absent, items={} order={:?}",
            queue_state.queue_items.len(),
            queue_state.shuffle_order,
        );
        return None;
    }
    let raw_order = raw_order.unwrap();

    let current_index =
        resolve_remote_start_index(queue_state, renderer_queue_item_id, renderer_track_id);
    let next_index = resolve_remote_start_index(
        queue_state,
        renderer_next_queue_item_id,
        renderer_next_track_id,
    );

    let mut ordered = Vec::with_capacity(queue_state.queue_items.len());
    if let Some(index) = current_index {
        ordered.push(index);
    }
    if let Some(index) = next_index {
        if !ordered.contains(&index) {
            ordered.push(index);
        }
    }
    for &index in raw_order {
        if !ordered.contains(&index) {
            ordered.push(index);
        }
    }
    for index in 0..queue_state.queue_items.len() {
        if !ordered.contains(&index) {
            ordered.push(index);
        }
    }

    log::debug!(
        "[QConnect] resolve_core_shuffle_order: result={:?} current={:?} next={:?}",
        ordered,
        current_index,
        next_index,
    );

    Some(ordered)
}

/// Does the head of this queue carry the cloud's PLACEHOLDER id rather than a
/// real one?
///
/// The cloud mints the head item of a freshly-pushed queue with the track's own
/// catalog id where the queue item id belongs, and every renderer has to report
/// that item back as 0. Observed on the wire as queue item ids
/// `[126886853, 10, 1]` over tracks `[126886853, 123452387, 126886854]`: the
/// tell is the head's id equalling its track id, and the later items — whose ids
/// are ordinary small ones — CORROBORATE it.
///
/// A release of exactly one track has no later item to corroborate with, and
/// demanding one anyway is what left a single release reporting its catalog id
/// where the controller expected 0. The controller cannot find that id in the
/// queue it holds, so the play button spun forever while the renderer sat paused
/// (vicrodh/qbz#794). Corroborate when there is something to corroborate with; a
/// queue of one rests on the head alone, which is safe because a catalog id is
/// never also a queue position.
fn is_cloud_placeholder_current_queue_item(
    queue: &QConnectQueueState,
    current_index: usize,
) -> bool {
    let Some(current_item) = queue.queue_items.get(current_index) else {
        return false;
    };

    if current_index != 0 || current_item.queue_item_id != current_item.track_id {
        return false;
    }

    queue.queue_items.len() == 1
        || queue
            .queue_items
            .iter()
            .skip(1)
            .any(|item| item.queue_item_id < current_item.queue_item_id)
}

pub fn normalize_current_queue_item_id_from_queue_state(
    queue: &QConnectQueueState,
    current_index: usize,
) -> u64 {
    if is_cloud_placeholder_current_queue_item(queue, current_index) {
        0
    } else {
        queue.queue_items[current_index].queue_item_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qconnect_core::QueueItem;

    fn item(queue_item_id: u64, track_id: u64) -> QueueItem {
        QueueItem {
            track_context_uuid: String::new(),
            track_id,
            queue_item_id,
        }
    }

    fn queue(items: Vec<QueueItem>) -> QConnectQueueState {
        QConnectQueueState {
            queue_items: items,
            ..Default::default()
        }
    }

    /// Regression (controller of a peer that was already rendering): a fresh album
    /// is pushed, but the cached renderer projection still names the PREVIOUS
    /// queue's item (qid=3 + the old track_id). The old track is absent from the
    /// new queue, so the stale qid must NOT resolve the cursor to the new queue's
    /// item 3 ("NowPlayingBar shows track 4 on a freshly-pushed album"). It falls
    /// through to the track_id lookup (miss) → None → caller defaults to the head.
    #[test]
    fn stale_qid_with_mismatched_track_does_not_land_on_wrong_track() {
        let q = queue(vec![
            item(0, 52848233),
            item(1, 52848234),
            item(2, 52848235),
            item(3, 52848236),
        ]);
        assert_eq!(
            resolve_remote_start_index(&q, Some(3), Some(126886856)),
            None
        );
    }

    /// A consistent projection (qid + the matching track at that position) still
    /// resolves to its index — the normal track-change / takeback path is unchanged.
    #[test]
    fn consistent_qid_and_track_resolves_to_index() {
        let q = queue(vec![item(0, 100), item(1, 200), item(2, 300)]);
        assert_eq!(resolve_remote_start_index(&q, Some(2), Some(300)), Some(2));
    }

    /// When no track is reported, the qid is trusted as before (some events carry
    /// only a queue_item_id).
    #[test]
    fn qid_without_track_id_is_trusted() {
        let q = queue(vec![item(0, 100), item(1, 200)]);
        assert_eq!(resolve_remote_start_index(&q, Some(1), None), Some(1));
    }

    /// An absent qid falls through to the track_id lookup.
    #[test]
    fn track_id_lookup_when_qid_absent_from_queue() {
        let q = queue(vec![item(0, 100), item(7, 200)]);
        assert_eq!(resolve_remote_start_index(&q, Some(99), Some(200)), Some(1));
    }

    /// Regression (vicrodh/qbz#794, "Blister Sunrise" by M83): a release of ONE
    /// track. Its head is the cloud's placeholder exactly as a longer release's
    /// is, and must normalize to 0 — there is simply no second item to prove it
    /// with. Reporting the catalog id instead left the controller's play button
    /// spinning forever.
    #[test]
    fn the_placeholder_head_of_a_one_track_release_still_normalizes_to_zero() {
        let q = queue(vec![item(126886853, 126886853)]);
        assert_eq!(normalize_current_queue_item_id_from_queue_state(&q, 0), 0);
    }

    /// The same shape a longer release arrives in, pinned beside it: ids
    /// `[126886853, 10, 1]` over tracks `[126886853, 123452387, 126886854]`, as
    /// captured from the wire.
    #[test]
    fn the_placeholder_head_of_a_longer_release_normalizes_to_zero() {
        let q = queue(vec![
            item(126886853, 126886853),
            item(10, 123452387),
            item(1, 126886854),
        ]);
        assert_eq!(normalize_current_queue_item_id_from_queue_state(&q, 0), 0);
        assert_eq!(normalize_current_queue_item_id_from_queue_state(&q, 1), 10);
    }

    /// A one-item queue whose head carries a REAL id is left alone: the tell is
    /// the id equalling the track id, and nothing else about a queue of one.
    #[test]
    fn a_one_item_queue_with_a_real_head_id_is_not_treated_as_a_placeholder() {
        let q = queue(vec![item(7, 126886853)]);
        assert_eq!(normalize_current_queue_item_id_from_queue_state(&q, 0), 7);
    }

    /// The cloud names the head by the id the renderer reports for it — 0 — and
    /// states no track. The raw ids hold the placeholder, so only the normalized
    /// id can match it. `find_cursor_index_by_queue_item_id` has always matched
    /// on both; this is the same rule on the start-index path.
    #[test]
    fn a_placeholder_head_resolves_when_the_cloud_names_it_by_zero() {
        let q = queue(vec![item(126886853, 126886853), item(1, 126886854)]);
        assert_eq!(resolve_remote_start_index(&q, Some(0), None), Some(0));
    }

    /// 0 is also a perfectly ordinary queue item id further down a queue, so an
    /// EXACT match wins over the normalized head.
    #[test]
    fn an_exact_zero_further_down_the_queue_beats_the_normalized_head() {
        let q = queue(vec![
            item(126886853, 126886853),
            item(1, 126886854),
            item(0, 126886855),
        ]);
        assert_eq!(resolve_remote_start_index(&q, Some(0), None), Some(2));
    }
}
