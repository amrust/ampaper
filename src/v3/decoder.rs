// v3 decoder — recover plaintext from a v3 blob.
//
// Phase 1 first slice: reads the 24-byte v3 header, deserializes
// the RaptorQ OTI, walks encoded packets in fixed strides, and
// feeds them to the RaptorQ decoder until it converges. Returns
// the recovered plaintext.

use raptorq::{Decoder, EncodingPacket, ObjectTransmissionInformation};

use super::format::{HEADER_LEN, MAGIC, OTI_LEN, PAYLOAD_ID_LEN, VERSION};

#[derive(Debug)]
pub enum DecodeError {
    /// Blob doesn't have enough bytes for the v3 header.
    TooShort { len: usize },
    /// First 8 bytes don't match `b"AMPAPER3"`. Most likely the
    /// caller routed a v1/v2 input here by mistake — check the
    /// magic before calling [`decode`].
    BadMagic,
    /// Version byte names a wire-format revision this decoder
    /// doesn't know how to read. Updating to a newer ampaper build
    /// is the fix.
    UnsupportedVersion { version: u8 },
    /// Reserved header bytes are non-zero. Either the blob is
    /// corrupted or it was produced by a future version that uses
    /// those bytes — refuse to guess which.
    NonZeroReserved,
    /// The packet stream length isn't an integer multiple of the
    /// per-packet wire size derived from the OTI's symbol size.
    /// Truncation, framing bug, or the wrong magic on the front of
    /// otherwise-arbitrary bytes.
    PacketStreamMisaligned { trailing_bytes: usize },
    /// RaptorQ exhausted the available packets without converging.
    /// In a paper-archive context this means too many cells were
    /// damaged or missing for the supplied repair budget.
    NoSolution,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort { len } => write!(
                f,
                "v3 decode: blob too short ({len} bytes; need ≥ {HEADER_LEN})"
            ),
            Self::BadMagic => f.write_str("v3 decode: not an ampaper v3 blob (magic mismatch)"),
            Self::UnsupportedVersion { version } => {
                write!(f, "v3 decode: unsupported version {version}")
            }
            Self::NonZeroReserved => {
                f.write_str("v3 decode: reserved header bytes must be zero in version 1")
            }
            Self::PacketStreamMisaligned { trailing_bytes } => write!(
                f,
                "v3 decode: packet stream misaligned ({trailing_bytes} trailing bytes)"
            ),
            Self::NoSolution => f.write_str(
                "v3 decode: RaptorQ did not converge — too few or too damaged packets",
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decode a v3 blob produced by [`encode`](super::encoder::encode).
///
/// Walks the encoded-packet stream until the RaptorQ decoder
/// returns a solution. Extra packets beyond what the decoder needs
/// are simply unused (the rateless property — no harm in feeding
/// the decoder all of them).
pub fn decode(blob: &[u8]) -> Result<Vec<u8>, DecodeError> {
    if blob.len() < HEADER_LEN {
        return Err(DecodeError::TooShort { len: blob.len() });
    }
    if &blob[0..8] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    if blob[8] != VERSION {
        return Err(DecodeError::UnsupportedVersion { version: blob[8] });
    }
    if blob[9..12].iter().any(|&b| b != 0) {
        return Err(DecodeError::NonZeroReserved);
    }

    let mut oti_buf = [0u8; OTI_LEN];
    oti_buf.copy_from_slice(&blob[12..24]);
    let oti = ObjectTransmissionInformation::deserialize(&oti_buf);

    let symbol_size = oti.symbol_size() as usize;
    let packet_size = PAYLOAD_ID_LEN + symbol_size;
    let packet_bytes = &blob[HEADER_LEN..];

    if packet_bytes.len() % packet_size != 0 {
        return Err(DecodeError::PacketStreamMisaligned {
            trailing_bytes: packet_bytes.len() % packet_size,
        });
    }

    let mut decoder = Decoder::new(oti);
    for chunk in packet_bytes.chunks_exact(packet_size) {
        let packet = EncodingPacket::deserialize(chunk);
        if let Some(plaintext) = decoder.decode(packet) {
            return Ok(plaintext);
        }
    }

    Err(DecodeError::NoSolution)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_too_short() {
        let err = decode(&[0u8; 4]).unwrap_err();
        assert!(matches!(err, DecodeError::TooShort { len: 4 }));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut blob = [0u8; HEADER_LEN];
        blob[..8].copy_from_slice(b"NOTAMP3!");
        let err = decode(&blob).unwrap_err();
        assert!(matches!(err, DecodeError::BadMagic));
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut blob = [0u8; HEADER_LEN];
        blob[..8].copy_from_slice(MAGIC);
        blob[8] = 99;
        let err = decode(&blob).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedVersion { version: 99 }));
    }

    #[test]
    fn rejects_nonzero_reserved() {
        let mut blob = [0u8; HEADER_LEN];
        blob[..8].copy_from_slice(MAGIC);
        blob[8] = VERSION;
        blob[10] = 0x01;
        let err = decode(&blob).unwrap_err();
        assert!(matches!(err, DecodeError::NonZeroReserved));
    }
}
