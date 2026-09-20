pub mod crypto;
pub mod error;
pub mod map;
pub mod parser;
pub mod vault;

pub use crypto::{compute_request_sig, decrypt_frame, derive_session_key, unwrap_content_key};
pub use error::CmafError;
pub use map::{Resume, SegmentMap};
pub use parser::{
    parse_init_segment, parse_segment_crypto, FrameEntry, InitInfo, SegmentCrypto,
    SegmentTableEntry,
};
pub use vault::{SealKey, SealReader, SealWriter, SEAL_HEADER_LEN};
