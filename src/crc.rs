// CRC-16 used by the v1 block-on-paper format. Per FORMAT-V1.md §2.2:
// - Polynomial: CCITT (0x1021)
// - Initial value: 0x0000
// - No reflection (input or output)
// - No final XOR
//
// This is the variant commonly known as CRC-16/XMODEM. The encoder
// then XORs the final value with 0x55AA before storing it in the
// block — see [`crate::block::Block::compute_crc`] for that step.
//
// Original C source: Crc16.cpp:50-90 in reference/paperbak-1.10.src/.
// The 256-entry lookup table there is hand-tabulated; we generate it
// at compile time from the polynomial and verify a handful of entries
// against the C source in tests.

const POLY: u16 = 0x1021;

/// CRC-16/CCITT lookup table (poly 0x1021, init 0, no reflection).
/// Built at compile time so a transcription typo from `Crc16.cpp`
/// is impossible.
const TABLE: [u16; 256] = build_table();

const fn build_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc: u16 = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ POLY
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// CRC-16/CCITT (poly 0x1021, init 0, no reflection, no final XOR)
/// over `data`. This is the bare CRC; the format adds its own
/// `0x55AA` final XOR on top of it before storing in the block.
///
/// Mirrors `Crc16.cpp:85-90`:
/// ```c
/// for (crc=0; length>0; length--)
///     crc=((crc<<8)^crctab[((crc>>8)^(*data++))]) & 0xFFFF;
/// ```
#[must_use]
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        let idx = ((crc >> 8) as u8 ^ byte) as usize;
        crc = (crc << 8) ^ TABLE[idx];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The empty input must produce the initial value (0x0000).
    /// Sanity check that the loop is a no-op when there's nothing to consume.
    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc16(b""), 0x0000);
    }

    /// Standard CRC-16/XMODEM check value. ASCII "123456789" must
    /// produce 0x31C3 — this is the published reference vector for
    /// CRC-16 with poly=0x1021, init=0x0000, no reflection, no XOR-out.
    /// If this fails the entire CRC variant is misconfigured.
    #[test]
    fn standard_check_vector() {
        assert_eq!(crc16(b"123456789"), 0x31C3);
    }

    /// Specific table entries transcribed from Crc16.cpp:50-82. The
    /// generated table must match the hand-tabulated one byte for byte.
    /// These cover the start, two interior points (chosen to span both
    /// halves of the table), and the last entry.
    #[test]
    fn table_entries_match_paperbak_source() {
        // Crc16.cpp:50: table[0..8] = { 0x0000, 0x1021, 0x2042, 0x3063, ... }
        assert_eq!(TABLE[0x00], 0x0000);
        assert_eq!(TABLE[0x01], 0x1021);
        assert_eq!(TABLE[0x07], 0x70E7);

        // Crc16.cpp:53: table[0x10..0x12] starts with 0x1231, 0x0210
        assert_eq!(TABLE[0x10], 0x1231);
        assert_eq!(TABLE[0x11], 0x0210);

        // Crc16.cpp:67: table[0x80] = 0x9188
        assert_eq!(TABLE[0x80], 0x9188);

        // Crc16.cpp:82: last entry table[0xFF] = 0x1EF0
        assert_eq!(TABLE[0xFF], 0x1EF0);
    }

    /// A single byte 0x01 must produce TABLE[0x01] = 0x1021. This pins
    /// the byte-fed loop to the table indexing convention so the index
    /// expression can't silently regress to e.g. just `byte as usize`.
    #[test]
    fn single_byte_matches_table_entry() {
        assert_eq!(crc16(&[0x01]), TABLE[0x01]);
        assert_eq!(crc16(&[0x80]), TABLE[0x80]);
        assert_eq!(crc16(&[0xFF]), TABLE[0xFF]);
    }

    /// CRC is order-sensitive: AB should not equal BA in general.
    /// This is mostly a smoke test that we're consuming bytes in
    /// sequence rather than e.g. xor-ing them together.
    #[test]
    fn order_matters() {
        assert_ne!(crc16(b"AB"), crc16(b"BA"));
    }
}
