//! A handle to one sealed track in the L2 cache.
//!
//! The path alone is no longer enough to read a cached track: the file is
//! sealed with a key that lives only in this process (see [`qbz_cmaf::vault`]),
//! so every read site needs the key as well. Carrying the two together is what
//! keeps that from becoming a key threaded through fifteen signatures — the
//! player hands a `CachedFile` around exactly where it used to hand a
//! `PathBuf`, and the four ways it reads a cached track all hang off this type.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use qbz_cmaf::{SealKey, SealReader};

/// One `<track_id>.audio` in the L2 cache, with the key that opens it.
#[derive(Clone, Debug)]
pub struct CachedFile {
    path: PathBuf,
    key: SealKey,
}

impl CachedFile {
    pub fn new(path: PathBuf, key: SealKey) -> Self {
        Self { path, key }
    }

    /// The file on the card. For logging and for existence checks — a caller
    /// that opens this directly gets ciphertext.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A reader over the AUDIO, in audio coordinates: offset 0 is the first
    /// FLAC byte and the seal header is invisible. Seekable, which is what
    /// symphonia's bisecting seek needs.
    pub fn open(&self) -> io::Result<SealReader<File>> {
        SealReader::new(self.key, File::open(&self.path)?)
    }

    /// The first `len` bytes of the audio, for the readers that parse a header
    /// — sample rate, bit depth, ReplayGain, all of which live in FLAC
    /// metadata blocks at the very start.
    ///
    /// `None` if the file cannot be read or is not sealed by this process; a
    /// SHORT read comes back as-is, since every caller either parses a header
    /// out of it or gives up gracefully.
    pub fn head(&self, len: usize) -> Option<Vec<u8>> {
        let mut reader = self
            .open()
            .map_err(|e| log::debug!("[CACHE] cannot open {}: {e}", self.path.display()))
            .ok()?;
        // Never allocate more than the track holds: `read_tag_head` asks for a
        // megabyte, and zeroing one for a file that is shorter than that is a
        // megabyte of a 512 MB board for nothing.
        let len = len.min(reader.len() as usize);
        let mut buf = vec![0u8; len];
        let mut filled = 0usize;
        while filled < len {
            match reader.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => return None,
            }
        }
        buf.truncate(filled);
        Some(buf)
    }

    /// The whole track in memory. A one-off on a user action (resume re-reads
    /// the track it handed to the decoder as a file) — never a resident cost,
    /// which is the entire reason the file case exists.
    pub fn read_all(&self) -> io::Result<Vec<u8>> {
        let mut reader = self.open()?;
        let mut out = Vec::with_capacity(reader.len() as usize);
        reader.read_to_end(&mut out)?;
        Ok(out)
    }
}
