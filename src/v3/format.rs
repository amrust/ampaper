// v3 wire format constants. See docs/FORMAT-V3.md for the full
// spec. Phase 1 first slice — only the bytes-level framing is
// defined here; cell layout, finder patterns, and bitmap rendering
// land in subsequent slices and bump the VERSION byte when their
// semantics break compatibility with this slice's reader.

/// 8-byte magic prefix that identifies an ampaper v3 blob. The
/// dispatcher uses this to route inputs to v3 vs the legacy
/// v1/v2 decoder. Distinct from any v1/v2 header so the legacy
/// decoder will never accidentally try to decode v3 output.
///
/// ASCII chosen so a `file(1)`-style sniff or a hex-dump of the
/// first bytes of any v3 blob immediately tells a human what
/// they're looking at.
pub const MAGIC: &[u8; 8] = b"AMPAPER3";

/// Format version byte. Phase 1 first slice = 1. Decoders MUST
/// reject unknown versions with [`DecodeError::UnsupportedVersion`]
/// rather than guessing.
///
/// [`DecodeError::UnsupportedVersion`]: super::decoder::DecodeError::UnsupportedVersion
pub const VERSION: u8 = 1;

/// Length of the fixed v3 header in bytes. Layout:
///
/// | Offset | Bytes | Field                                  |
/// |--------|-------|----------------------------------------|
/// | 0      | 8     | Magic = `b"AMPAPER3"`                  |
/// | 8      | 1     | Version (currently 1)                  |
/// | 9      | 3     | Reserved — must be zero in version 1   |
/// | 12     | 12    | RaptorQ OTI (per RFC 6330 §3.3.2)      |
///
/// Followed by a stream of RaptorQ encoded packets in their
/// RFC 6330 wire form (4-byte payload ID + T-byte symbol). Symbol
/// size T is encoded in the OTI; the decoder reads T from there
/// and walks packets in `T + 4` byte strides.
pub const HEADER_LEN: usize = 24;

/// RaptorQ OTI on-wire size, per RFC 6330 §3.3.2.
pub(crate) const OTI_LEN: usize = 12;

/// RaptorQ payload-ID size on-wire (1-byte SBN + 3-byte ESI),
/// per RFC 6330 §3.2.
pub(crate) const PAYLOAD_ID_LEN: usize = 4;
