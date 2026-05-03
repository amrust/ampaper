// v3 encoder — RaptorQ-frame plaintext bytes into a v3 blob.
//
// Phase 1 first slice: encodes a byte buffer to a single blob with
// the v3 magic header + RaptorQ OTI + a stream of encoded packets.
// No bitmap rendering yet, no compression, no encryption.
//
// RaptorQ recovery property (Qualcomm, "RaptorQ Technical Overview"):
//   reconstruction probability after receiving K + h packets =
//   1 - 1/256^(h+1)
// where K is the source-symbol count and h is the receive overhead
// beyond K. For paper-archive use the channel is "decades on a
// shelf" — the loss profile is patchy fade + edge tear hitting a
// modest fraction of cells, so even a few repair packets give
// effectively perfect recovery. The default [`EncodeOptions`]
// setting (10 repair packets) is a starting point until the cell
// layer lands and we measure real scanner loss profiles.

use raptorq::Encoder;

use super::format::{HEADER_LEN, MAGIC, OTI_LEN, VERSION};

#[derive(Debug)]
pub enum EncodeError {
    /// Empty input — RaptorQ rejects 0-byte source objects, and an
    /// empty payload would round-trip to nothing useful anyway.
    EmptyInput,
}

impl core::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyInput => f.write_str("v3 encode: empty input"),
        }
    }
}

impl std::error::Error for EncodeError {}

/// v3 encode parameters. Defaults are tuned for the bytes-level
/// first slice; the cell-layer phase will introduce per-cell
/// payload sizing that picks `mtu` automatically based on the
/// chosen physical density.
#[derive(Clone, Copy, Debug)]
pub struct EncodeOptions {
    /// MTU hint passed to RaptorQ. Determines the symbol size T
    /// per RFC 6330 §4.4.1.2. Each on-wire encoded packet then
    /// carries a 4-byte payload ID + T bytes of symbol data.
    ///
    /// On paper this maps to bytes-per-cell; the cell-layer phase
    /// will set `mtu` to the per-cell payload capacity (32×32 dot
    /// cell minus per-cell overhead).
    pub mtu: u16,

    /// Number of repair packets to generate beyond the source K.
    /// More repair = more loss tolerance, more bytes on disk/paper.
    /// At least 1 is required for any meaningful loss recovery;
    /// 0 produces a systematic-only output where every packet must
    /// survive the round-trip.
    pub repair_packets: u32,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            // 256-byte symbols. Small enough that even small inputs
            // produce many packets (so RaptorQ's group structure
            // works in its sweet spot), large enough that header +
            // payload-ID overhead stays a small fraction of the
            // wire bytes. Will be revisited when the cell-layer
            // phase pins the per-cell payload capacity.
            mtu: 256,
            // 10 repair packets — modest but non-trivial loss
            // tolerance for the bytes-level slice. The cell-layer
            // phase will tune this against measured scanner loss
            // profiles.
            repair_packets: 10,
        }
    }
}

/// Encode `plaintext` to a v3 blob. The output has the layout:
///
/// ```text
///   [MAGIC: 8] [VERSION: 1] [reserved: 3] [OTI: 12]
///   [packet 1: 4 + T bytes]
///   [packet 2: 4 + T bytes]
///   ...
/// ```
///
/// Packets are RaptorQ EncodingPackets in their RFC 6330 wire form.
/// Symbol size T is stored in the OTI; the decoder reads T and
/// walks packets in fixed-size strides.
pub fn encode(plaintext: &[u8], options: &EncodeOptions) -> Result<Vec<u8>, EncodeError> {
    if plaintext.is_empty() {
        return Err(EncodeError::EmptyInput);
    }

    let encoder = Encoder::with_defaults(plaintext, options.mtu);
    let oti_bytes: [u8; OTI_LEN] = encoder.get_config().serialize();
    let packets = encoder.get_encoded_packets(options.repair_packets);

    // Pre-size the output: header + (each packet is payload-ID +
    // symbol). Symbol size from OTI may differ slightly from MTU
    // due to alignment, but mtu is a tight upper bound.
    let mut out = Vec::with_capacity(
        HEADER_LEN + packets.len() * (4 + options.mtu as usize),
    );

    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&[0u8; 3]);
    out.extend_from_slice(&oti_bytes);
    debug_assert_eq!(out.len(), HEADER_LEN);

    for packet in &packets {
        out.extend_from_slice(&packet.serialize());
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_rejected() {
        let err = encode(&[], &EncodeOptions::default()).unwrap_err();
        assert!(matches!(err, EncodeError::EmptyInput));
    }

    #[test]
    fn output_starts_with_magic_and_version() {
        let blob = encode(b"hi", &EncodeOptions::default()).unwrap();
        assert_eq!(&blob[0..8], MAGIC);
        assert_eq!(blob[8], VERSION);
        assert_eq!(&blob[9..12], &[0, 0, 0]);
    }
}
