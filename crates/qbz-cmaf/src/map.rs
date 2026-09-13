//! Assembled-stream geometry: which segment carries which byte.
//!
//! The init segment's table pairs a `byte_len` with a `sample_count` for every
//! audio segment, and [`crate::decrypt_frame`] + the assembly in
//! `qbz_qobuz::cmaf::decrypt_segment_into` turn segment `k` into exactly
//! `byte_len[k - 1]` bytes of the finished FLAC stream — AES-CTR is a stream
//! cipher, so decryption preserves length, and the assembler emits the whole
//! `mdat` payload and nothing else.
//!
//! That makes the table a seek index, not merely a size estimate: running
//! prefix sums over `byte_len`, offset by the FLAC header the init segment
//! also carries, give the assembled byte offset of every segment boundary
//! before a single audio byte has been fetched.
//!
//! # Why this is trusted
//!
//! Measured, not assumed. `download_full_sized`'s straight-to-disk branch has
//! long refused to publish a file whose byte count differs from
//! `flac_header.len() + sum(byte_len)`, and on hardware it published a
//! 33-segment 24/96 track at 108,906,763 bytes — the declared figure to the
//! byte — which `flac -t` then verified against the MD5 in its own STREAMINFO.
//! An estimate is wrong by kilobytes; this is wrong by zero, on a stream whose
//! every sample is independently accounted for.
//!
//! It is still checked at run time rather than taken on faith: the feeder
//! compares each decrypted segment against [`SegmentMap::segment_len`] and
//! fails the track loudly on a mismatch. Wrong offsets would otherwise be
//! served as if they were right.

use crate::parser::SegmentTableEntry;

/// Where a feeder must restart its body to serve a given assembled offset.
///
/// A segment is the smallest decryptable unit — every frame in it is needed to
/// walk the frame table — so a seek lands on the segment boundary at or before
/// the target, never exactly on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resume {
    /// Assembled offset of the first byte the feeder will push. Always less
    /// than or equal to the offset asked for.
    pub body_offset: u64,
    /// Whether the FLAC header has to be pushed before the first segment.
    pub with_header: bool,
    /// 1-based index of the first segment to fetch. One past the last segment
    /// means there is nothing left to fetch.
    pub first_segment: usize,
}

/// Assembled-FLAC offsets for one track's segment table.
///
/// `segment_start(k)` is `flac_header.len() + sum(byte_len[0..k - 1])`, with
/// segments numbered from 1 as the CDN numbers them (segment 0 is the init
/// segment, which carries the header rather than audio).
#[derive(Debug, Clone)]
pub struct SegmentMap {
    /// Assembled offset of each segment boundary: `starts[0]` is the start of
    /// segment 1 (= the header length) and `starts[n]` is the total assembled
    /// length. `n + 1` entries for `n` segments.
    starts: Vec<u64>,
}

impl SegmentMap {
    /// Build the map from the FLAC header length and the init segment's table.
    ///
    /// `table[i]` describes segment `i + 1`.
    pub fn new(header_len: usize, table: &[SegmentTableEntry]) -> Self {
        let mut starts = Vec::with_capacity(table.len() + 1);
        let mut at = header_len as u64;
        starts.push(at);
        for entry in table {
            at += entry.byte_len as u64;
            starts.push(at);
        }
        Self { starts }
    }

    /// Number of audio segments the map covers.
    pub fn segment_count(&self) -> usize {
        self.starts.len() - 1
    }

    /// Bytes of FLAC header in front of segment 1.
    pub fn header_len(&self) -> u64 {
        self.starts[0]
    }

    /// Total assembled size: header plus every segment.
    pub fn total_len(&self) -> u64 {
        self.starts[self.starts.len() - 1]
    }

    /// Assembled offset where segment `seg` (1-based) begins.
    pub fn segment_start(&self, seg: usize) -> Option<u64> {
        (1..=self.segment_count())
            .contains(&seg)
            .then(|| self.starts[seg - 1])
    }

    /// Assembled length of segment `seg` (1-based) — its declared `byte_len`.
    pub fn segment_len(&self, seg: usize) -> Option<u64> {
        (1..=self.segment_count())
            .contains(&seg)
            .then(|| self.starts[seg] - self.starts[seg - 1])
    }

    /// 1-based segment holding `offset`, or `None` for an offset inside the
    /// FLAC header or past the end of the stream.
    pub fn segment_at(&self, offset: u64) -> Option<usize> {
        if offset < self.header_len() || offset >= self.total_len() {
            return None;
        }
        // `starts` is non-decreasing, so the last boundary at or below
        // `offset` is the segment that carries it. A zero-length segment
        // (never seen in the wild, but the table is a u32 the server fills)
        // is skipped over rather than returned, which is what
        // `partition_point` does for free.
        Some(self.starts.partition_point(|&start| start <= offset))
    }

    /// What a feeder asked for `offset` must fetch, and where its first pushed
    /// byte belongs.
    pub fn resume_at(&self, offset: u64) -> Resume {
        if offset < self.header_len() {
            return Resume {
                body_offset: 0,
                with_header: true,
                first_segment: 1,
            };
        }
        match self.segment_at(offset) {
            Some(seg) => Resume {
                body_offset: self.starts[seg - 1],
                with_header: false,
                first_segment: seg,
            },
            // Past the end: nothing left to fetch.
            None => Resume {
                body_offset: self.total_len(),
                with_header: false,
                first_segment: self.segment_count() + 1,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(lens: &[u32]) -> Vec<SegmentTableEntry> {
        lens.iter()
            .map(|&byte_len| SegmentTableEntry {
                byte_len,
                sample_count: 4096,
            })
            .collect()
    }

    #[test]
    fn starts_are_the_running_prefix_sums() {
        let map = SegmentMap::new(42, &table(&[100, 200, 300]));
        assert_eq!(map.segment_count(), 3);
        assert_eq!(map.header_len(), 42);
        assert_eq!(map.segment_start(1), Some(42));
        assert_eq!(map.segment_start(2), Some(142));
        assert_eq!(map.segment_start(3), Some(342));
        assert_eq!(map.segment_start(4), None);
        assert_eq!(map.total_len(), 642);
    }

    #[test]
    fn segment_len_is_the_declared_byte_len() {
        let map = SegmentMap::new(42, &table(&[100, 200, 300]));
        assert_eq!(map.segment_len(1), Some(100));
        assert_eq!(map.segment_len(2), Some(200));
        assert_eq!(map.segment_len(3), Some(300));
        assert_eq!(map.segment_len(0), None);
        assert_eq!(map.segment_len(4), None);
    }

    /// Every byte of the stream maps to the segment that actually carries it,
    /// checked exhaustively rather than at the boundaries only — an off-by-one
    /// here sends the feeder to the wrong segment and the reader gets bytes
    /// attributed to offsets they do not belong to.
    #[test]
    fn every_offset_maps_to_its_own_segment() {
        let map = SegmentMap::new(5, &table(&[3, 1, 4]));
        for offset in 0..map.total_len() {
            let seg = map.segment_at(offset);
            match seg {
                None => assert!(offset < map.header_len(), "offset {offset} is audio"),
                Some(seg) => {
                    let start = map.segment_start(seg).expect("in range");
                    let len = map.segment_len(seg).expect("in range");
                    assert!(
                        offset >= start && offset < start + len,
                        "offset {offset} placed in segment {seg} [{start}, {})",
                        start + len
                    );
                }
            }
        }
        assert_eq!(map.segment_at(map.total_len()), None);
    }

    #[test]
    fn a_resume_lands_on_the_segment_boundary_at_or_before_the_target() {
        let map = SegmentMap::new(42, &table(&[100, 200, 300]));
        for offset in 0..map.total_len() {
            let resume = map.resume_at(offset);
            assert!(
                resume.body_offset <= offset,
                "resume for {offset} started at {} — ahead of the target",
                resume.body_offset
            );
            if resume.with_header {
                assert_eq!(resume.body_offset, 0);
                assert_eq!(resume.first_segment, 1);
                assert!(offset < map.header_len());
            } else {
                assert_eq!(
                    map.segment_start(resume.first_segment),
                    Some(resume.body_offset)
                );
                // Never more than one segment early: that is the bound the
                // seek path is allowed to pay.
                let len = map.segment_len(resume.first_segment).expect("in range");
                assert!(offset < resume.body_offset + len);
            }
        }
    }

    #[test]
    fn a_resume_past_the_end_has_nothing_to_fetch() {
        let map = SegmentMap::new(42, &table(&[100, 200]));
        let resume = map.resume_at(map.total_len());
        assert_eq!(resume.first_segment, map.segment_count() + 1);
        assert_eq!(resume.body_offset, map.total_len());
        assert!(!resume.with_header);
    }

    /// A table the server sent with no segments at all still answers every
    /// question without panicking — `starts` always has its header entry.
    #[test]
    fn an_empty_table_is_a_header_and_nothing_else() {
        let map = SegmentMap::new(42, &[]);
        assert_eq!(map.segment_count(), 0);
        assert_eq!(map.total_len(), 42);
        assert_eq!(map.segment_at(0), None);
        assert_eq!(map.segment_at(41), None);
        assert_eq!(map.resume_at(0).first_segment, 1);
    }
}
