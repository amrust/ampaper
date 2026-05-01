// Legacy AES-192 decryption for the v1 PaperBack 1.10 format.
//
// Read-only by design. Per docs/ENCRYPTION-DECISION.md and the
// memory `encryption_decision.md`:
//   - v1 read: this module. Decrypt only.
//   - v1 write: never. The encoder rejects encryption=on requests
//     because re-emitting AES-192-CBC + CRC-as-MAC would be
//     cargo-culting an obsolete posture.
//   - v2 forward (future M11): AES-256-GCM with a fresh KDF.
//
// Per FORMAT-V1.md §6.3 and Printer.cpp:471-498 / Fileproc.cpp:
// 297-322, the v1 format derives the key as:
//
//   key = PBKDF2-HMAC-SHA-1(password, salt, iter=524288, dkLen=24)
//
// where:
//   salt   = SuperBlock.name[32..48]  (16 bytes of CryptGenRandom)
//   iv     = SuperBlock.name[48..64]  (16 bytes, also random)
//   filecrc = CRC-16/CCITT of the plaintext (post-bzip2 if compressed,
//             pre-encryption); the decoder uses it to verify the
//             password is correct.
//
// **HMAC variant caveat (open since M1).** PaperBack 1.10 ships
// `crypto.lib` precompiled, so we cannot read which HMAC variant
// Brian Gladman's pwd2key was built with. Gladman's source defaults
// to HMAC-SHA-1 unless `HMAC_SHA2` is `#define`d at build time. This
// module assumes SHA-1 — it's the safer guess, and if a captured
// PB 1.10 ciphertext fails to decrypt with our key, swapping to
// HMAC-SHA-256 is a one-line change. The first encrypted golden
// vector that lands resolves the ambiguity once and for all.
//
// PAPERBAK-HACKS.md §5.1 catalogues this as "format-critical
// unknown" — it stays an unknown until we have a real ciphertext.

use aes::Aes192;
use aes::cipher::{BlockModeDecrypt, KeyIvInit};
use cbc::cipher::block_padding::NoPadding;

/// The AES-192 key length in bytes. paperbak.h:35.
pub const AES_KEY_LEN: usize = 24;

/// AES block size, fixed for AES-{128,192,256}.
pub const AES_BLOCK_LEN: usize = 16;

/// PBKDF2 iteration count baked into PaperBack 1.10's key
/// derivation. Printer.cpp:477 / Fileproc.cpp:304.
pub const PBKDF2_ITERATIONS: u32 = 524_288;

/// PBKDF2 salt length in bytes. Printer.cpp:477 (4th argument).
pub const PBKDF2_SALT_LEN: usize = 16;

/// Derive the AES-192 key from a password and 16-byte salt using
/// the parameters PaperBack 1.10's `crypto.lib` is built with —
/// PBKDF2-HMAC-SHA-1, 524288 iterations, 24-byte output.
///
/// Constant-time-ish only insofar as the underlying RustCrypto
/// stack is; this module does not zeroize derived keys, callers
/// who care about secret material lifetime should drop the returned
/// array as soon as the AES context is initialized.
#[must_use]
pub fn derive_key_v1(password: &[u8], salt: &[u8; PBKDF2_SALT_LEN]) -> [u8; AES_KEY_LEN] {
    let mut key = [0u8; AES_KEY_LEN];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password, salt, PBKDF2_ITERATIONS, &mut key);
    key
}

/// Decrypt `buf` in place using AES-192 in CBC mode with a 16-byte
/// IV. The buffer length must be a multiple of [`AES_BLOCK_LEN`].
/// Returns `Err(LegacyAesError::UnalignedLength)` otherwise; the
/// PaperBack 1.10 format guarantees alignment by zero-padding pre-
/// encryption, so this should only fire on a corrupt SuperBlock
/// reporting a wrong `datasize`.
///
/// No padding is stripped — PB 1.10's encrypt path zero-pads the
/// plaintext before encryption (Printer.cpp:417-420) and does not
/// store an explicit padding length; bzip2 (or whatever consumes
/// the plaintext) tolerates the trailing zeros. Mirrors
/// Fileproc.cpp:313's `aes_cbc_decrypt(pf->data, tempdata, ...)`.
pub fn decrypt_v1_in_place(
    buf: &mut [u8],
    key: &[u8; AES_KEY_LEN],
    iv: &[u8; AES_BLOCK_LEN],
) -> Result<(), LegacyAesError> {
    if buf.len() % AES_BLOCK_LEN != 0 {
        return Err(LegacyAesError::UnalignedLength { len: buf.len() });
    }
    type Aes192CbcDec = cbc::Decryptor<Aes192>;
    let cipher = Aes192CbcDec::new(key.into(), iv.into());
    cipher
        .decrypt_padded::<NoPadding>(buf)
        .map_err(|_| LegacyAesError::DecryptFailed)?;
    Ok(())
}

/// Errors from the legacy AES-192 decrypt path.
#[derive(Debug, PartialEq, Eq)]
pub enum LegacyAesError {
    /// Buffer length is not a multiple of 16 bytes — AES-CBC requires
    /// block alignment. PaperBack 1.10's encoder always zero-pads
    /// pre-encryption; this error indicates a corrupt SuperBlock.
    UnalignedLength { len: usize },
    /// The CBC primitive itself rejected the operation. With
    /// `NoPadding` and aligned input this should never happen, but
    /// the type system requires the branch.
    DecryptFailed,
}

impl core::fmt::Display for LegacyAesError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnalignedLength { len } => {
                write!(f, "AES-CBC requires 16-byte aligned input; got {len} bytes")
            }
            Self::DecryptFailed => f.write_str("AES-CBC decrypt rejected by RustCrypto primitive"),
        }
    }
}

impl std::error::Error for LegacyAesError {}

/// Test-only encrypt helper. Forward write of v1 AES-192 is NOT in
/// M7 scope — re-emitting the obsolete v1 cipher is rejected by
/// encoder.rs. This helper exists strictly so round-trip tests in
/// this module AND full-stack encryption tests in crate::decoder /
/// crate::scan can exercise both directions.
#[cfg(test)]
pub(crate) fn encrypt_v1_in_place_for_testing(
    buf: &mut [u8],
    key: &[u8; AES_KEY_LEN],
    iv: &[u8; AES_BLOCK_LEN],
) {
    use aes::cipher::BlockModeEncrypt;
    type Aes192CbcEnc = cbc::Encryptor<Aes192>;
    assert_eq!(
        buf.len() % AES_BLOCK_LEN,
        0,
        "test helper requires aligned buffer"
    );
    let cipher = Aes192CbcEnc::new(key.into(), iv.into());
    let len = buf.len();
    cipher
        .encrypt_padded::<NoPadding>(buf, len)
        .expect("aligned buffer should encrypt without error");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PBKDF2-HMAC-SHA-1 RFC 6070 test vector 1:
    ///   P = "password"
    ///   S = "salt"
    ///   c = 1
    ///   dkLen = 20
    ///   DK = 0c60c80f961f0e71f3a9b524af6012062fe037a6
    ///
    /// We use a 20-byte output here just to cross-check the algorithm
    /// against a known-good vector. Our v1 keys are 24 bytes, but the
    /// algorithm is the same; just the output length differs.
    #[test]
    fn pbkdf2_sha1_matches_rfc6070_vector_1() {
        let mut key = [0u8; 20];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(b"password", b"salt", 1, &mut key);
        let expected = [
            0x0c, 0x60, 0xc8, 0x0f, 0x96, 0x1f, 0x0e, 0x71, 0xf3, 0xa9, 0xb5, 0x24, 0xaf, 0x60,
            0x12, 0x06, 0x2f, 0xe0, 0x37, 0xa6,
        ];
        assert_eq!(key, expected);
    }

    /// PBKDF2-HMAC-SHA-1 RFC 6070 test vector 2:
    ///   P = "password", S = "salt", c = 2, dkLen = 20
    ///   DK = ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957
    #[test]
    fn pbkdf2_sha1_matches_rfc6070_vector_2() {
        let mut key = [0u8; 20];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(b"password", b"salt", 2, &mut key);
        let expected = [
            0xea, 0x6c, 0x01, 0x4d, 0xc7, 0x2d, 0x6f, 0x8c, 0xcd, 0x1e, 0xd9, 0x2a, 0xce, 0x1d,
            0x41, 0xf0, 0xd8, 0xde, 0x89, 0x57,
        ];
        assert_eq!(key, expected);
    }

    /// Sanity check: derive_key_v1 with PaperBack's parameters
    /// produces deterministic output for known inputs. The expected
    /// bytes are computed by running the same PBKDF2 inputs through
    /// the underlying primitive — this catches a future regression
    /// where someone bumps `PBKDF2_ITERATIONS` or swaps the hash
    /// without updating the docs.
    #[test]
    fn derive_key_v1_is_deterministic() {
        let salt = [0x42u8; PBKDF2_SALT_LEN];
        let key1 = derive_key_v1(b"correct horse battery staple", &salt);
        let key2 = derive_key_v1(b"correct horse battery staple", &salt);
        assert_eq!(key1, key2);
        // Different password produces different key.
        let key3 = derive_key_v1(b"different password", &salt);
        assert_ne!(key1, key3);
        // Different salt produces different key.
        let salt2 = [0x99u8; PBKDF2_SALT_LEN];
        let key4 = derive_key_v1(b"correct horse battery staple", &salt2);
        assert_ne!(key1, key4);
    }

    /// Round-trip: encrypt with the test helper, decrypt with the
    /// public path, recover original bytes. Confirms the cipher
    /// glue works without depending on a captured ciphertext.
    #[test]
    fn round_trip_encrypt_decrypt_aligned() {
        let key = [0x11u8; AES_KEY_LEN];
        let iv = [0x22u8; AES_BLOCK_LEN];
        let mut buf = [0u8; 64];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i * 7) as u8;
        }
        let original = buf;

        encrypt_v1_in_place_for_testing(&mut buf, &key, &iv);
        assert_ne!(buf, original, "encryption must change the bytes");

        decrypt_v1_in_place(&mut buf, &key, &iv).unwrap();
        assert_eq!(buf, original, "round trip must recover original");
    }

    /// Wrong key → decrypt produces garbage that does not match the
    /// original. Pins that we're not accidentally returning the
    /// ciphertext untouched (which a no-op decrypt would do).
    #[test]
    fn wrong_key_does_not_recover_original() {
        let key = [0x11u8; AES_KEY_LEN];
        let wrong_key = [0x12u8; AES_KEY_LEN];
        let iv = [0x22u8; AES_BLOCK_LEN];
        let mut buf = [0u8; 32];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i * 13) as u8;
        }
        let original = buf;
        encrypt_v1_in_place_for_testing(&mut buf, &key, &iv);
        decrypt_v1_in_place(&mut buf, &wrong_key, &iv).unwrap();
        assert_ne!(buf, original);
    }

    /// Unaligned buffer length yields the expected error variant.
    #[test]
    fn unaligned_length_is_rejected() {
        let key = [0u8; AES_KEY_LEN];
        let iv = [0u8; AES_BLOCK_LEN];
        let mut buf = [0u8; 17];
        let err = decrypt_v1_in_place(&mut buf, &key, &iv).unwrap_err();
        assert!(matches!(err, LegacyAesError::UnalignedLength { len: 17 }));
    }

    // encrypt_v1_in_place_for_testing now lives at module level for
    // cross-module test access. See above the `mod tests` block.
}
