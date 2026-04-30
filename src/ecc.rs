// Reed-Solomon error correction for the v1 block-on-paper format.
//
// Per FORMAT-V1.md §2.3, the format uses a CCSDS-style (255,223,33)
// Reed-Solomon code over GF(256), shortened by virtual padding of
// 127 zero symbols to a (128, 96) shortened code: 96 message bytes
// (addr + data + crc, the first 96 bytes of every block on the wire)
// followed by 32 parity bytes (the `ecc` field). The parity bytes
// can correct up to 16 byte errors per block.
//
// GF(256) parameters:
// - primitive polynomial: 0x187 (x^8 + x^7 + x^2 + x + 1)
// - primitive element α = 2
// - first consecutive root (FCR): 112
// - primitive root step: 11
//
// All of these are non-default for an off-the-shelf RS implementation,
// so we hand-roll the math rather than depend on a crate that assumes
// FCR=0/1 and step=1. The algorithm is a direct port of Phil Karn's
// `rs.c` (KA9Q) as included in PaperBack 1.10's `Ecc.cpp`. See
// PAPERBAK-HACKS.md §4.1 — `goto finish` and other 1990s-C structure
// is rewritten idiomatically; the bit math is unchanged.

// --- Format constants --------------------------------------------------------

/// Reed-Solomon parity bytes per block (= 255 - 223 = n - k of the
/// unshortened code). Same value as [`crate::block::ECC_BYTES`].
pub const PARITY_BYTES: usize = 32;

/// Number of virtual zero symbols prepended before the message during
/// shortening. The unshortened (255, 223) code is converted to a
/// (128, 96) shortened code by pretending the first PAD bytes of the
/// codeword are always zero.
pub const PAD: usize = 127;

/// Bytes of message (data) the shortened code carries: 223 - PAD = 96.
pub const MESSAGE_BYTES: usize = 223 - PAD;

/// Bytes of codeword on the wire: 255 - PAD = 128. Equal to
/// [`crate::block::BLOCK_BYTES`].
pub const CODEWORD_BYTES: usize = 255 - PAD;

/// Maximum number of byte errors the code can correct per block.
/// (n - k) / 2 = 32 / 2 = 16.
pub const MAX_CORRECTABLE_ERRORS: usize = PARITY_BYTES / 2;

// First consecutive root used to build the generator polynomial.
// `Ecc.cpp:126` (decode) and `Ecc.cpp:101` (encode use poly[]).
const FCR: u8 = 112;
// Primitive root step between consecutive roots.
const PRIM: u8 = 11;

// --- GF(256) tables ----------------------------------------------------------

/// Build the antilog and log tables for GF(256) with the primitive
/// polynomial x^8 + x^7 + x^2 + x + 1 = 0x187 and primitive element α = 2.
///
/// `alpha[i] = α^i` for i in 0..255; `alpha[255] = 0` is a sentinel
/// (Karn's convention so `index[v] == 255` flags "v is zero" inside
/// arithmetic). `index[v] = i such that alpha[i] = v` for v in 1..256;
/// `index[0] = 255` (the same sentinel).
const fn build_tables() -> ([u8; 256], [u8; 256]) {
    let mut alpha = [0u8; 256];
    let mut index = [0u8; 256];
    alpha[0] = 1;
    index[1] = 0;
    let mut i = 1;
    while i < 255 {
        let prev = alpha[i - 1];
        // Multiply by α (= 2): left-shift, reduce by primitive poly
        // (the low 8 bits, 0x87) when the high bit overflows.
        let next = if prev & 0x80 != 0 {
            (prev << 1) ^ 0x87
        } else {
            prev << 1
        };
        alpha[i] = next;
        index[next as usize] = i as u8;
        i += 1;
    }
    // alpha[255] stays 0; index[0] = 255 (sentinel for "zero").
    index[0] = 255;
    (alpha, index)
}

const TABLES: ([u8; 256], [u8; 256]) = build_tables();
const ALPHA: [u8; 256] = TABLES.0;
const INDEX: [u8; 256] = TABLES.1;

/// Generator polynomial coefficients in log form (α exponents).
/// `g(x) = ∏_{j=0..32} (x − α^(FCR + PRIM*j))`. Transcribed from
/// `Ecc.cpp:85-91`; symmetric (poly[k] == poly[32-k]) because of the
/// FCR=112 / step=11 choice that makes g self-reciprocal.
const POLY: [u8; PARITY_BYTES + 1] = [
    0, 249, 59, 66, 4, 43, 126, 251, 97, 30, 3, 213, 50, 66, 170, 5, 24, 5, 170, 66, 50, 213, 3,
    30, 97, 251, 126, 43, 4, 66, 59, 249, 0,
];

// --- Encoder -----------------------------------------------------------------

/// Compute the 32-byte Reed-Solomon parity for a 96-byte message.
/// Mirrors `Ecc.cpp:93-111`. The data slice must be exactly
/// [`MESSAGE_BYTES`] (96) long; this is enforced at runtime since
/// callers typically pass a slice carved from a larger buffer.
///
/// # Panics
/// Panics if `data.len() != MESSAGE_BYTES`.
#[must_use]
pub fn encode_parity(data: &[u8]) -> [u8; PARITY_BYTES] {
    assert_eq!(
        data.len(),
        MESSAGE_BYTES,
        "encode_parity expects exactly {MESSAGE_BYTES} message bytes"
    );
    let mut bb = [0u8; PARITY_BYTES];
    for &byte in data {
        let feedback = INDEX[(byte ^ bb[0]) as usize];
        if feedback != 255 {
            for j in 1..PARITY_BYTES {
                let exp = (feedback as usize + POLY[PARITY_BYTES - j] as usize) % 255;
                bb[j] ^= ALPHA[exp];
            }
        }
        // Shift bb left by one byte: bb[0..31] = bb[1..32]; bb[31] = new tail.
        bb.copy_within(1..PARITY_BYTES, 0);
        bb[PARITY_BYTES - 1] = if feedback != 255 {
            ALPHA[(feedback as usize + POLY[0] as usize) % 255]
        } else {
            0
        };
    }
    bb
}

// --- Decoder -----------------------------------------------------------------

/// Outcome of a Reed-Solomon decode attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The codeword has more errors than the code can correct
    /// ([`MAX_CORRECTABLE_ERRORS`] is the limit). Mirrors the C
    /// source's `count = -1` / "deg_lambda != count" sentinel.
    Uncorrectable,
}

/// Decode a 128-byte Reed-Solomon codeword in place, correcting up
/// to [`MAX_CORRECTABLE_ERRORS`] byte errors. Returns the number of
/// errors corrected on success, or [`DecodeError::Uncorrectable`].
///
/// Mirrors `Ecc.cpp:113-236`. The decoder is the standard
/// Berlekamp-Massey + Chien search + Forney algorithm pipeline as
/// implemented by Phil Karn; the only PaperBack-specific detail is
/// the FCR=112 / PRIM=11 weighting in syndrome computation.
///
/// # Panics
/// Panics if `data.len() != CODEWORD_BYTES`.
pub fn decode8(data: &mut [u8]) -> Result<usize, DecodeError> {
    assert_eq!(
        data.len(),
        CODEWORD_BYTES,
        "decode8 expects exactly {CODEWORD_BYTES} codeword bytes"
    );

    // --- Syndromes (Ecc.cpp:119-129) ----------------------------------------
    // S_i = data evaluated at α^((FCR+i)*PRIM), for i in 0..32 — the
    // generator polynomial roots inherited from PaperBack's choice
    // of `poly[]`. Note this is `(FCR+i)*PRIM`, NOT `FCR + i*PRIM`;
    // the multiplication is over both terms. Forward Horner: start
    // with s[i] = data[0] and fold in each subsequent byte, treating
    // data[0] as the high-order coefficient.
    let mut weights = [0usize; PARITY_BYTES];
    for (i, w) in weights.iter_mut().enumerate() {
        *w = ((FCR as usize + i) * PRIM as usize) % 255;
    }
    let mut s = [0u8; PARITY_BYTES];
    for slot in &mut s {
        *slot = data[0];
    }
    for &byte in &data[1..] {
        for (i, slot) in s.iter_mut().enumerate() {
            *slot = if *slot == 0 {
                byte
            } else {
                byte ^ ALPHA[(INDEX[*slot as usize] as usize + weights[i]) % 255]
            };
        }
    }

    // Convert syndromes to log form; if all are zero, no errors.
    let mut syn_error = 0u8;
    for slot in &mut s {
        syn_error |= *slot;
        *slot = INDEX[*slot as usize];
    }
    if syn_error == 0 {
        return Ok(0);
    }

    // --- Berlekamp-Massey (Ecc.cpp:136-181) --------------------------------
    // Find error-locator polynomial λ(x) and the related b(x).
    // Sizes are PARITY_BYTES+1 = 33 to cover the leading constant term.
    let mut lambda = [0u8; PARITY_BYTES + 1];
    let mut b = [0u8; PARITY_BYTES + 1];
    let mut t = [0u8; PARITY_BYTES + 1];
    lambda[0] = 1;
    for i in 0..PARITY_BYTES + 1 {
        b[i] = INDEX[lambda[i] as usize];
    }
    let mut el: usize = 0;
    let mut r: usize = 0;
    while r < PARITY_BYTES {
        r += 1;
        // Compute the discrepancy at step r.
        let mut discr_r: u8 = 0;
        for i in 0..r {
            if lambda[i] != 0 && s[r - i - 1] != 255 {
                let exp = (INDEX[lambda[i] as usize] as usize + s[r - i - 1] as usize) % 255;
                discr_r ^= ALPHA[exp];
            }
        }
        let discr_r_log = INDEX[discr_r as usize];
        if discr_r_log == 255 {
            // Discrepancy is zero: shift b right by one position.
            b.copy_within(0..PARITY_BYTES, 1);
            b[0] = 255;
        } else {
            // λ_new = λ - discr * b   (in GF arithmetic, − is XOR)
            t[0] = lambda[0];
            for i in 0..PARITY_BYTES {
                t[i + 1] = if b[i] != 255 {
                    lambda[i + 1] ^ ALPHA[(discr_r_log as usize + b[i] as usize) % 255]
                } else {
                    lambda[i + 1]
                };
            }
            if 2 * el < r {
                el = r - el;
                for i in 0..=PARITY_BYTES {
                    b[i] = if lambda[i] == 0 {
                        255
                    } else {
                        ((INDEX[lambda[i] as usize] as i32 - discr_r_log as i32 + 255) % 255) as u8
                    };
                }
            } else {
                b.copy_within(0..PARITY_BYTES, 1);
                b[0] = 255;
            }
            lambda.copy_from_slice(&t);
        }
    }

    // Convert λ to log form, find its degree.
    let mut deg_lambda: usize = 0;
    for i in 0..=PARITY_BYTES {
        lambda[i] = INDEX[lambda[i] as usize];
        if lambda[i] != 255 {
            deg_lambda = i;
        }
    }

    // --- Chien search (Ecc.cpp:186-200) ------------------------------------
    // Find roots of λ(x) by brute-force evaluation at α^k for k stepping
    // through the codeword positions. Step is 116 = (255 - PRIM) mod 255
    // — the inverse of α^PRIM in the multiplicative group.
    let mut reg = [0u8; PARITY_BYTES + 1];
    reg[1..=PARITY_BYTES].copy_from_slice(&lambda[1..=PARITY_BYTES]);
    let mut count: usize = 0;
    let mut root = [0u8; PARITY_BYTES];
    let mut loc = [0u8; PARITY_BYTES];
    let mut k: u8 = 115;
    for i in 1..=255u16 {
        let mut q: u8 = 1;
        for j in (1..=deg_lambda).rev() {
            if reg[j] != 255 {
                reg[j] = ((reg[j] as usize + j) % 255) as u8;
                q ^= ALPHA[reg[j] as usize];
            }
        }
        if q == 0 {
            root[count] = i as u8;
            loc[count] = k;
            count += 1;
            if count == deg_lambda {
                break;
            }
        }
        k = ((k as usize + 116) % 255) as u8;
    }
    if deg_lambda != count {
        return Err(DecodeError::Uncorrectable);
    }

    // --- Forney's algorithm (Ecc.cpp:204-231) ------------------------------
    // Compute error magnitudes from λ(x) and the syndromes via
    // ω(x) = s(x) λ(x) mod x^32, then error_i = ω(α^-i) / λ'(α^-i).
    let deg_omega = deg_lambda - 1;
    let mut omega = [0u8; PARITY_BYTES + 1];
    for i in 0..=deg_omega {
        let mut tmp: u8 = 0;
        for j in (0..=i).rev() {
            if s[i - j] != 255 && lambda[j] != 255 {
                tmp ^= ALPHA[(s[i - j] as usize + lambda[j] as usize) % 255];
            }
        }
        omega[i] = INDEX[tmp as usize];
    }
    for j in (0..count).rev() {
        // Numerator: ω(α^root[j]).
        let mut num1: u8 = 0;
        for i in (0..=deg_omega).rev() {
            if omega[i] != 255 {
                num1 ^= ALPHA[(omega[i] as usize + i * root[j] as usize) % 255];
            }
        }
        // Constant term in Forney's formula. The C `(root[j]*111+255)%255`
        // gathers the FCR-1 = 111 weight for our parameters.
        let num2 = ALPHA[(root[j] as usize * 111 + 255) % 255];
        // Denominator: λ'(α^root[j]) ≈ formal derivative; for binary fields
        // the even-power terms vanish, so we walk only odd-degree terms.
        let mut den: u8 = 0;
        let stop = (if deg_lambda < 31 { deg_lambda } else { 31 }) & !1;
        let mut i = stop as isize;
        while i >= 0 {
            if lambda[(i + 1) as usize] != 255 {
                den ^= ALPHA
                    [(lambda[(i + 1) as usize] as usize + i as usize * root[j] as usize) % 255];
            }
            i -= 2;
        }
        // Apply correction. Skip positions inside the virtual padding.
        if num1 != 0 && (loc[j] as usize) >= PAD {
            let pos = loc[j] as usize - PAD;
            let exp = (INDEX[num1 as usize] as usize + INDEX[num2 as usize] as usize + 255
                - INDEX[den as usize] as usize)
                % 255;
            data[pos] ^= ALPHA[exp];
        }
    }

    Ok(count)
}

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// alpha[7] = 0x80, alpha[8] = 0x87 are the canonical first
    /// "wrap" values of the primitive poly 0x187. If table generation
    /// is off by one or uses the wrong reduction, this trips loudly.
    /// Cross-check entries against Ecc.cpp:15-48.
    #[test]
    fn alpha_table_matches_paperbak_source() {
        // Ecc.cpp:15-16: alpha[0..16] = {0x01,0x02,0x04,...,0x80,0x87,0x89,...}
        assert_eq!(ALPHA[0], 0x01);
        assert_eq!(ALPHA[1], 0x02);
        assert_eq!(ALPHA[7], 0x80);
        assert_eq!(ALPHA[8], 0x87);
        assert_eq!(ALPHA[9], 0x89);
        assert_eq!(ALPHA[15], 0xF4);
        // Ecc.cpp:47: last 8 entries end with 0xC3, 0x00.
        assert_eq!(ALPHA[254], 0xC3);
        assert_eq!(ALPHA[255], 0x00);
    }

    /// index[v] is the inverse of alpha[i]. For every nonzero v,
    /// alpha[index[v]] == v must hold.
    #[test]
    fn index_table_is_alpha_inverse() {
        for v in 1u16..=255 {
            assert_eq!(ALPHA[INDEX[v as usize] as usize], v as u8, "v = {v}");
        }
        assert_eq!(INDEX[0], 255, "index[0] sentinel must be 255");
    }

    /// poly is symmetric for FCR=112 / PRIM=11. Pin a few entries
    /// against Ecc.cpp:85-91 to catch transcription typos.
    #[test]
    fn generator_poly_matches_paperbak_source() {
        assert_eq!(POLY[0], 0);
        assert_eq!(POLY[1], 249);
        assert_eq!(POLY[2], 59);
        assert_eq!(POLY[16], 24);
        assert_eq!(POLY[31], 249);
        assert_eq!(POLY[32], 0);
        // Symmetric-around-16 property:
        for k in 0..=16 {
            assert_eq!(POLY[k], POLY[32 - k], "asymmetric at k={k}");
        }
    }

    /// Encoding then decoding (no errors injected) must return 0
    /// corrections and leave the codeword bit-identical.
    #[test]
    fn round_trip_zero_errors() {
        let mut codeword = [0u8; CODEWORD_BYTES];
        // Fill the message portion with a non-trivial pattern.
        for (i, b) in codeword[..MESSAGE_BYTES].iter_mut().enumerate() {
            *b = (i * 37 + 5) as u8;
        }
        let parity = encode_parity(&codeword[..MESSAGE_BYTES]);
        codeword[MESSAGE_BYTES..].copy_from_slice(&parity);

        let original = codeword;
        let n_corrected = decode8(&mut codeword).unwrap();
        assert_eq!(n_corrected, 0);
        assert_eq!(codeword, original);
    }

    /// Empty message (all zeros) must produce all-zero parity.
    /// Useful sanity check on the encoder shift register.
    #[test]
    fn zero_message_has_zero_parity() {
        let parity = encode_parity(&[0u8; MESSAGE_BYTES]);
        assert_eq!(parity, [0u8; PARITY_BYTES]);
    }

    /// One-byte error in the message must be correctable; corrected
    /// codeword must equal the original; correction count must be 1.
    #[test]
    fn corrects_single_byte_error_in_message() {
        let mut codeword = build_codeword(|i| (i as u8).wrapping_mul(7));
        let original = codeword;
        codeword[42] ^= 0xA5;
        let n = decode8(&mut codeword).unwrap();
        assert_eq!(n, 1);
        assert_eq!(codeword, original);
    }

    /// One-byte error in the parity tail must also be correctable.
    #[test]
    fn corrects_single_byte_error_in_parity() {
        let mut codeword = build_codeword(|i| (i as u8).wrapping_add(0x33));
        let original = codeword;
        codeword[120] ^= 0x77; // parity region (96..128)
        let n = decode8(&mut codeword).unwrap();
        assert_eq!(n, 1);
        assert_eq!(codeword, original);
    }

    /// 16 byte errors at varied positions must still be correctable
    /// (the (32, 16) BCH bound). Use a deterministic pattern so the
    /// test is reproducible.
    #[test]
    fn corrects_max_byte_errors() {
        let mut codeword = build_codeword(|i| (i as u8).wrapping_mul(13).wrapping_add(91));
        let original = codeword;
        let positions = [
            3, 11, 18, 25, 33, 47, 58, 67, 71, 80, 89, 97, 105, 112, 119, 126,
        ];
        assert_eq!(positions.len(), MAX_CORRECTABLE_ERRORS);
        for &p in &positions {
            codeword[p] ^= 0xFF;
        }
        let n = decode8(&mut codeword).unwrap();
        assert_eq!(n, MAX_CORRECTABLE_ERRORS);
        assert_eq!(codeword, original);
    }

    /// 17 byte errors exceeds the correction capacity. The decoder
    /// must either signal Uncorrectable or — if it still gives an
    /// answer — produce a wrong codeword (i.e. NOT silently report
    /// success on garbage). The contract documented in DecodeError
    /// is explicit: error counts above MAX_CORRECTABLE_ERRORS are
    /// not recoverable.
    #[test]
    fn beyond_max_errors_is_uncorrectable_or_wrong() {
        let mut codeword = build_codeword(|i| (i as u8) ^ 0x5A);
        let original = codeword;
        let positions = [
            2, 9, 17, 24, 31, 39, 46, 54, 61, 69, 76, 84, 91, 99, 106, 114, 121,
        ];
        assert_eq!(positions.len(), MAX_CORRECTABLE_ERRORS + 1);
        for &p in &positions {
            codeword[p] ^= 0xFF;
        }
        let outcome = decode8(&mut codeword);
        match outcome {
            Err(DecodeError::Uncorrectable) => {
                // Acceptable.
            }
            Ok(_) => {
                // Decoder declared success; the decoded codeword must
                // NOT equal the original (otherwise the (32,16) bound
                // is being silently exceeded, which would be a bug).
                assert_ne!(codeword, original);
            }
        }
    }

    fn build_codeword(message_byte: impl Fn(usize) -> u8) -> [u8; CODEWORD_BYTES] {
        let mut codeword = [0u8; CODEWORD_BYTES];
        for (i, b) in codeword[..MESSAGE_BYTES].iter_mut().enumerate() {
            *b = message_byte(i);
        }
        let parity = encode_parity(&codeword[..MESSAGE_BYTES]);
        codeword[MESSAGE_BYTES..].copy_from_slice(&parity);
        codeword
    }

    // ---- Cross-source vectors against PaperBack 1.10's Ecc.cpp -------------
    //
    // Captured by compiling PaperBack 1.10's Encode8 / Decode8 stand-alone
    // and emitting parity for three deterministic inputs. The build harness
    // lives at reference/helpers/ecc_vectors.c (gitignored under /reference/);
    // re-run it after any change that could affect encoder output:
    //
    //   cd reference/helpers
    //   cl /nologo /O2 /TP ecc_vectors.c ../paperbak-1.10.src/Ecc.cpp /Fe:ecc_vectors.exe
    //   ./ecc_vectors.exe
    //
    // Two assertions per vector:
    //   1. Our encoder produces the same parity bytes as the C encoder
    //      (catches encoder drift against the spec).
    //   2. Our decoder, given the C-emitted codeword, returns 0 corrections
    //      and leaves the codeword unchanged (catches decoder drift).
    //
    // Together these are level-3 of the three-way cross-check from
    // feedback_three_way_crosscheck.md memory: the third party (PaperBack
    // 1.10's own C source) decoded ampaper's "legacy mode" output equivalent.

    /// Pure ramp 0..96. Lowest-entropy non-trivial input.
    const VECTOR_RAMP_PARITY: [u8; PARITY_BYTES] = [
        0xAC, 0xEF, 0x22, 0x7E, 0x64, 0x50, 0x76, 0x60, 0x6F, 0x2A, 0xD3, 0xAA, 0xDF, 0x88, 0xF7,
        0x08, 0xAA, 0x9D, 0x69, 0x9D, 0x56, 0xB5, 0x37, 0xE3, 0xB1, 0x1D, 0x77, 0x4F, 0x55, 0x61,
        0x77, 0x8A,
    ];

    /// LCG-style pattern: i * 37 + 5 mod 256. Higher-entropy than the ramp.
    const VECTOR_LCG_PARITY: [u8; PARITY_BYTES] = [
        0x05, 0xCF, 0xF8, 0x7B, 0x9E, 0xD4, 0xA1, 0xA6, 0x08, 0x6C, 0xEE, 0xA3, 0x50, 0x48, 0x47,
        0x84, 0xDD, 0x28, 0x47, 0x38, 0x04, 0x0A, 0xE4, 0x89, 0xB7, 0x59, 0xAA, 0xD9, 0x7F, 0xCC,
        0xDB, 0xFB,
    ];

    /// Block-shaped: addr = LE(0x0000_5A00), data = 0xC3 ^ index, crc = 0x55AA.
    /// Mirrors the byte layout of a real Block::to_bytes()[..96] for a data
    /// block at offset 0x5A00, so this vector also exercises the bytes the
    /// encoder will see in practice.
    const VECTOR_BLOCK_PARITY: [u8; PARITY_BYTES] = [
        0x4C, 0xC1, 0x09, 0x3C, 0x64, 0xC8, 0xFE, 0x6C, 0xA5, 0x6E, 0x9A, 0xAC, 0x93, 0x68, 0xFD,
        0x19, 0x7B, 0x52, 0xAA, 0x56, 0x2F, 0xB2, 0xB2, 0xF7, 0x90, 0xD7, 0xE5, 0xE5, 0xA2, 0x45,
        0x06, 0x20,
    ];

    fn check_vector(message: &[u8; MESSAGE_BYTES], expected_parity: &[u8; PARITY_BYTES]) {
        // Encoder must match the C source byte for byte.
        let parity = encode_parity(message);
        assert_eq!(
            &parity, expected_parity,
            "Rust parity diverged from C parity"
        );

        // Decoder must accept the C-emitted codeword with zero corrections.
        let mut codeword = [0u8; CODEWORD_BYTES];
        codeword[..MESSAGE_BYTES].copy_from_slice(message);
        codeword[MESSAGE_BYTES..].copy_from_slice(expected_parity);
        let original = codeword;
        let n = decode8(&mut codeword).expect("decoder rejected valid C-emitted codeword");
        assert_eq!(
            n, 0,
            "decoder reported non-zero corrections on valid codeword"
        );
        assert_eq!(codeword, original, "decoder mutated a valid codeword");
    }

    #[test]
    fn matches_paperbak_c_source_ramp_vector() {
        let mut msg = [0u8; MESSAGE_BYTES];
        for (i, b) in msg.iter_mut().enumerate() {
            *b = i as u8;
        }
        check_vector(&msg, &VECTOR_RAMP_PARITY);
    }

    #[test]
    fn matches_paperbak_c_source_lcg_vector() {
        let mut msg = [0u8; MESSAGE_BYTES];
        for (i, b) in msg.iter_mut().enumerate() {
            *b = (i as u32 * 37 + 5) as u8;
        }
        check_vector(&msg, &VECTOR_LCG_PARITY);
    }

    #[test]
    fn matches_paperbak_c_source_block_shaped_vector() {
        let mut msg = [0u8; MESSAGE_BYTES];
        // addr = 0x0000_5A00 (LE)
        msg[0] = 0x00;
        msg[1] = 0x5A;
        msg[2] = 0x00;
        msg[3] = 0x00;
        // data[0..90] = 0xC3 ^ index
        for i in 0..NDATA_TEST {
            msg[4 + i] = 0xC3 ^ (i as u8);
        }
        // crc bytes 94..96 = LE 0x55AA
        msg[94] = 0xAA;
        msg[95] = 0x55;
        check_vector(&msg, &VECTOR_BLOCK_PARITY);
    }

    /// Local copy of NDATA so this module doesn't have to reach into block.
    /// 90 bytes of data per block — comes from FORMAT-V1.md / paperbak.h:51.
    const NDATA_TEST: usize = 90;
    const _: () = assert!(4 + NDATA_TEST + 2 == MESSAGE_BYTES);
}
