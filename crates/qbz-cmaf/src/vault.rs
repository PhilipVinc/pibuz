//! Sealing the L2 disk cache.
//!
//! Everything the daemon writes to the card is DECRYPTED audio: the CMAF path
//! decrypts each segment before it assembles, and the legacy path downloads a
//! plain FLAC to begin with. Left as-is, `~/.cache/qbz/playback/` is a folder of
//! playable Hi-Res FLACs. This module is what stops that: every byte that
//! reaches the cache is AES-128-CTR'd first, and nothing outside this process
//! can turn a `<id>.audio` back into audio.
//!
//! # The key is ephemeral, and that is the point
//!
//! [`SealKey::random`] is called once per `PlaybackCache`, which is once per
//! daemon start. It never touches the disk. A key file beside the ciphertext
//! would only be obfuscation — anyone holding the card would hold both halves —
//! whereas a key that dies with the process means the files left on the card
//! after a `killall pibuz` are not audio by any route, including ours.
//!
//! The cost is that the L2 cache no longer survives a restart, so the cache
//! directory is WIPED at startup (see `PlaybackCache::with_path`). On moOde
//! that is every renderer toggle, because `stopQobuz()` kills the daemon. This
//! was chosen deliberately over a persistent key.
//!
//! # Why CTR
//!
//! The cache is seeked, not streamed: symphonia bisects the file to serve a
//! seek, `read_head` wants the first 256 KB of a 220 MB track, and resume
//! re-reads from an arbitrary offset. CTR is a keystream, so plaintext byte `p`
//! is always at file byte `p + SEAL_HEADER_LEN` and decrypting it needs only
//! the counter at `p` — no chaining, no block alignment, no re-reading from the
//! start. A cipher with a block mode would have made every one of those reads a
//! whole-file decrypt.
//!
//! Key reuse is not a concern the way it usually is with CTR: the key is fresh
//! per process AND the 16-byte nonce is fresh per file, so no two files, and no
//! two runs, share a keystream.

use std::io::{self, Read, Seek, SeekFrom, Write};

use aes::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use rand::Rng;

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// `b"PIBUZSL1"` — present on every sealed file, checked on every open. A file
/// without it is from a build that wrote plaintext, and is swept rather than
/// read.
const MAGIC: [u8; 8] = *b"PIBUZSL1";

const VERSION: u16 = 1;

/// Bytes of header in front of the ciphertext: magic, version, reserved, nonce.
///
/// Public because the cache accounts in PLAINTEXT bytes — a sealed file is this
/// much longer than the audio it holds, and the size checks either side of the
/// cache have to agree about which number they mean.
pub const SEAL_HEADER_LEN: u64 = 32;

/// The process-lifetime key every cached file is sealed with.
///
/// `Copy` on purpose: it is handed to whatever is about to open or write a
/// cache file — the disk tee, the straight-to-disk downloader, every read site
/// in the player — and a key behind an `Arc<Mutex<..>>` would buy nothing that
/// 16 bytes on the stack does not.
#[derive(Clone, Copy)]
pub struct SealKey([u8; 16]);

impl SealKey {
    /// A fresh key. Called once per cache, never persisted.
    pub fn random() -> Self {
        let mut key = [0u8; 16];
        rand::rng().fill_bytes(&mut key);
        Self(key)
    }
}

/// Deliberately opaque: a key that prints is a key in the log, and
/// `qbz_log::register_secret` cannot redact what it was never told about.
impl std::fmt::Debug for SealKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SealKey(<redacted>)")
    }
}

fn cipher_at(key: &SealKey, nonce: &[u8; 16], plaintext_pos: u64) -> Aes128Ctr {
    let mut c = Aes128Ctr::new(&key.0.into(), nonce.into());
    // Infallible for any offset a track can reach: the counter is 128 bits and
    // the largest track is ~220 MB.
    c.seek(plaintext_pos);
    c
}

fn header_bytes(nonce: &[u8; 16]) -> [u8; SEAL_HEADER_LEN as usize] {
    let mut head = [0u8; SEAL_HEADER_LEN as usize];
    head[..8].copy_from_slice(&MAGIC);
    head[8..10].copy_from_slice(&VERSION.to_le_bytes());
    // 10..16 stay zero — room to grow the header without changing its length.
    head[16..].copy_from_slice(nonce);
    head
}

fn parse_header(head: &[u8; SEAL_HEADER_LEN as usize]) -> io::Result<[u8; 16]> {
    if head[..8] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a sealed cache file",
        ));
    }
    let version = u16::from_le_bytes([head[8], head[9]]);
    if version != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("sealed cache file version {version} is not readable by this build"),
        ));
    }
    let mut nonce = [0u8; 16];
    nonce.copy_from_slice(&head[16..]);
    Ok(nonce)
}

/// Does `head` begin a sealed file?
///
/// For the startup sweep, which has to tell a file this build wrote from a
/// plaintext one an older build left behind.
pub fn looks_sealed(head: &[u8]) -> bool {
    head.len() >= MAGIC.len() && head[..MAGIC.len()] == MAGIC
}

/// Seals a byte stream as it is written, header first.
///
/// Sequential only — which is all four writers into the cache are. The counter
/// advances with the bytes handed over, so a caller that skipped or rewound
/// would seal at the wrong offsets; there is no API here to do that with.
pub struct SealWriter<W: Write> {
    inner: W,
    key: SealKey,
    nonce: [u8; 16],
    pos: u64,
    /// One scratch buffer rather than an allocation per chunk. The cipher works
    /// in place and the caller's slice is borrowed, so the bytes have to be
    /// copied somewhere before they can be encrypted.
    scratch: Vec<u8>,
}

impl<W: Write> SealWriter<W> {
    /// Write the header and take ownership of `inner`.
    pub fn new(key: SealKey, mut inner: W) -> io::Result<Self> {
        let mut nonce = [0u8; 16];
        rand::rng().fill_bytes(&mut nonce);
        inner.write_all(&header_bytes(&nonce))?;
        Ok(Self {
            inner,
            key,
            nonce,
            pos: 0,
            scratch: Vec::new(),
        })
    }

    /// Plaintext bytes sealed so far — NOT the size of the file, which is this
    /// plus [`SEAL_HEADER_LEN`].
    pub fn plaintext_len(&self) -> u64 {
        self.pos
    }

    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for SealWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all(buf)?;
        Ok(buf.len())
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        self.scratch.clear();
        self.scratch.extend_from_slice(buf);
        cipher_at(&self.key, &self.nonce, self.pos).apply_keystream(&mut self.scratch);
        self.inner.write_all(&self.scratch)?;
        self.pos += buf.len() as u64;
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Reads a sealed file back as plaintext, in plaintext coordinates.
///
/// Offset 0 is the first audio byte; the header is invisible to the caller.
/// That is what lets a `SealReader` stand in for a `File` at every read site
/// the L2 cache has — the decoder, the tag head, the resume re-read and
/// symphonia's bisecting seek all address the audio, not the file.
pub struct SealReader<R: Read + Seek> {
    inner: R,
    key: SealKey,
    nonce: [u8; 16],
    /// Plaintext cursor. Tracked here rather than derived from the inner file
    /// on every read: one `seek(Current(0))` per `read` would be a syscall per
    /// 4 KB of a 220 MB track.
    pos: u64,
    len: u64,
}

impl<R: Read + Seek> SealReader<R> {
    /// Validate the header and position at the first audio byte.
    ///
    /// Fails on a file that is not sealed, which is how a plaintext leftover
    /// from an older build surfaces as an error rather than as noise handed to
    /// the decoder.
    pub fn new(key: SealKey, mut inner: R) -> io::Result<Self> {
        let file_len = inner.seek(SeekFrom::End(0))?;
        if file_len < SEAL_HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "sealed cache file is shorter than its header",
            ));
        }
        inner.rewind()?;
        let mut head = [0u8; SEAL_HEADER_LEN as usize];
        inner.read_exact(&mut head)?;
        let nonce = parse_header(&head)?;
        Ok(Self {
            inner,
            key,
            nonce,
            pos: 0,
            len: file_len - SEAL_HEADER_LEN,
        })
    }

    /// Plaintext length: the size of the audio, not of the file.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<R: Read + Seek> Read for SealReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            cipher_at(&self.key, &self.nonce, self.pos).apply_keystream(&mut buf[..n]);
            self.pos += n as u64;
        }
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for SealReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        // Resolved against the PLAINTEXT length, then shifted: `End(-4)` means
        // four bytes before the end of the audio, not of the file.
        let target = match from {
            SeekFrom::Start(n) => n as i64,
            SeekFrom::End(n) => self.len as i64 + n,
            SeekFrom::Current(n) => self.pos as i64 + n,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before the start of the sealed stream",
            ));
        }
        let target = target as u64;
        self.inner.seek(SeekFrom::Start(target + SEAL_HEADER_LEN))?;
        self.pos = target;
        Ok(target)
    }
}

/// Seal `plaintext` into a whole buffer, for the caller that has the bytes in
/// hand already (`PlaybackCache::insert`, spilling an L1 track).
pub fn seal_to_vec(key: SealKey, plaintext: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 16];
    rand::rng().fill_bytes(&mut nonce);
    let mut out = Vec::with_capacity(SEAL_HEADER_LEN as usize + plaintext.len());
    out.extend_from_slice(&header_bytes(&nonce));
    out.extend_from_slice(plaintext);
    cipher_at(&key, &nonce, 0).apply_keystream(&mut out[SEAL_HEADER_LEN as usize..]);
    out
}

/// The reverse of [`seal_to_vec`], in place, for a whole file already read.
pub fn unseal_in_place(key: SealKey, mut sealed: Vec<u8>) -> io::Result<Vec<u8>> {
    if sealed.len() < SEAL_HEADER_LEN as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sealed cache file is shorter than its header",
        ));
    }
    let mut head = [0u8; SEAL_HEADER_LEN as usize];
    head.copy_from_slice(&sealed[..SEAL_HEADER_LEN as usize]);
    let nonce = parse_header(&head)?;
    sealed.drain(..SEAL_HEADER_LEN as usize);
    cipher_at(&key, &nonce, 0).apply_keystream(&mut sealed);
    Ok(sealed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Deterministic pseudo-audio: a plaintext where every byte position is
    /// identifiable, so a seek that lands one byte out is visible.
    fn plaintext(len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        v.extend_from_slice(b"fLaC");
        let mut x: u32 = 0x1234_5678;
        while v.len() < len {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            v.push((x >> 24) as u8);
        }
        v.truncate(len);
        v
    }

    fn seal_through_writer(key: SealKey, data: &[u8], chunk: usize) -> Vec<u8> {
        let mut w = SealWriter::new(key, Cursor::new(Vec::new())).expect("header");
        for part in data.chunks(chunk) {
            w.write_all(part).expect("write");
        }
        assert_eq!(w.plaintext_len(), data.len() as u64);
        w.into_inner().into_inner()
    }

    #[test]
    fn a_sealed_file_round_trips_through_the_writer_and_reader() {
        let key = SealKey::random();
        let data = plaintext(70_001);
        // Chunked at a size that is neither block- nor buffer-aligned: the
        // counter has to carry across writes, and 16 would hide it.
        let sealed = seal_through_writer(key, &data, 4097);

        assert_eq!(sealed.len() as u64, data.len() as u64 + SEAL_HEADER_LEN);
        let mut r = SealReader::new(key, Cursor::new(sealed)).expect("open");
        assert_eq!(r.len(), data.len() as u64);
        let mut back = Vec::new();
        r.read_to_end(&mut back).expect("read");
        assert_eq!(back, data);
    }

    #[test]
    fn whole_buffer_sealing_matches_the_streaming_writer() {
        let key = SealKey::random();
        let data = plaintext(9_000);
        let sealed = seal_to_vec(key, &data);
        let back = unseal_in_place(key, sealed.clone()).expect("unseal");
        assert_eq!(back, data);

        // And the two writers are interchangeable at the READER, which is what
        // matters: `insert` and the disk tee both produce files the same open
        // path has to read.
        let mut r = SealReader::new(key, Cursor::new(sealed)).expect("open");
        let mut back = Vec::new();
        r.read_to_end(&mut back).expect("read");
        assert_eq!(back, data);
    }

    /// The property the whole design rests on: plaintext byte `p` is readable
    /// without touching byte `p - 1`. Symphonia bisects a 220 MB file to serve
    /// a seek, and `read_head` wants the first 256 KB of one.
    #[test]
    fn a_seek_lands_on_the_byte_it_names() {
        let key = SealKey::random();
        let data = plaintext(50_000);
        let sealed = seal_through_writer(key, &data, 8192);
        let mut r = SealReader::new(key, Cursor::new(sealed)).expect("open");

        // Offsets that are not multiples of the 16-byte AES block, in an order
        // that goes both forwards and backwards.
        for &at in &[0u64, 1, 15, 16, 17, 4095, 49_999, 33, 20_001, 7] {
            assert_eq!(r.seek(SeekFrom::Start(at)).expect("seek"), at);
            let mut buf = [0u8; 1];
            r.read_exact(&mut buf).expect("read");
            assert_eq!(buf[0], data[at as usize], "byte at {at}");
        }

        // Relative and end-anchored seeks address the AUDIO, not the file.
        assert_eq!(r.seek(SeekFrom::End(-4)).expect("seek"), 49_996);
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).expect("read");
        assert_eq!(tail, data[49_996..]);

        r.seek(SeekFrom::Start(100)).expect("seek");
        r.seek(SeekFrom::Current(50)).expect("seek");
        let mut one = [0u8; 1];
        r.read_exact(&mut one).expect("read");
        assert_eq!(one[0], data[150]);
    }

    /// What the change is FOR: what lands on the card is not the audio.
    #[test]
    fn the_sealed_bytes_are_not_the_plaintext() {
        let key = SealKey::random();
        let data = plaintext(20_000);
        let sealed = seal_through_writer(key, &data, 4096);
        let body = &sealed[SEAL_HEADER_LEN as usize..];

        assert_ne!(body, &data[..], "the body went out in the clear");
        assert!(
            !body.windows(4).any(|w| w == b"fLaC"),
            "a FLAC signature survived into the sealed file"
        );
    }

    /// Same key, same bytes, twice — two different files. The nonce is per
    /// file, so a cache full of the same track twice leaks nothing by
    /// comparison, and the L1 spill writing a track the tee already wrote
    /// cannot reuse a keystream.
    #[test]
    fn two_files_never_share_a_keystream() {
        let key = SealKey::random();
        let data = plaintext(4_096);
        let a = seal_through_writer(key, &data, 1024);
        let b = seal_through_writer(key, &data, 1024);
        assert_ne!(a, b, "two seals of one track produced identical files");
        assert_ne!(
            &a[SEAL_HEADER_LEN as usize..],
            &b[SEAL_HEADER_LEN as usize..]
        );
    }

    /// A key from another run does not open the file. There is no error to
    /// detect it by — CTR always "works" — so this pins the only thing that is
    /// true: what comes out is not the audio.
    #[test]
    fn another_runs_key_does_not_recover_the_audio() {
        let data = plaintext(8_192);
        let sealed = seal_through_writer(SealKey::random(), &data, 2048);
        let mut r = SealReader::new(SealKey::random(), Cursor::new(sealed)).expect("open");
        let mut back = Vec::new();
        r.read_to_end(&mut back).expect("read");
        assert_eq!(back.len(), data.len());
        assert_ne!(back, data);
    }

    /// A plaintext file from a build before this one must not be handed to the
    /// decoder as if it were sealed. The startup sweep uses the same check.
    #[test]
    fn a_plaintext_file_is_recognised_and_refused() {
        let data = plaintext(1_024);
        assert!(!looks_sealed(&data));
        let err = match SealReader::new(SealKey::random(), Cursor::new(data.clone())) {
            Ok(_) => panic!("a plaintext file opened as sealed"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let sealed = seal_to_vec(SealKey::random(), &data);
        assert!(looks_sealed(&sealed));
        assert!(!looks_sealed(&sealed[..4]), "a short read is not a verdict");
    }

    #[test]
    fn a_truncated_file_is_refused_rather_than_decoded() {
        let key = SealKey::random();
        let sealed = seal_to_vec(key, &plaintext(1_000));
        let stub = sealed[..8].to_vec();
        assert!(SealReader::new(key, Cursor::new(stub)).err().is_some());
        assert!(unseal_in_place(key, sealed[..8].to_vec()).is_err());
    }

    #[test]
    fn an_empty_track_seals_and_reads_back_empty() {
        let key = SealKey::random();
        let sealed = seal_through_writer(key, &[], 512);
        assert_eq!(sealed.len() as u64, SEAL_HEADER_LEN);
        let r = SealReader::new(key, Cursor::new(sealed)).expect("open");
        assert!(r.is_empty());
    }
}
