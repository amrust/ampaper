// AES-256-GCM authenticated encryption + PBKDF2-HMAC-SHA-256 KDF for
// the v2 forward-emit format. Spec: docs/FORMAT-V2.md §3 and the
// rationale at docs/ENCRYPTION-DECISION.md.
//
// Three primitives this module owns:
//   - derive_key_v2(password, salt) → 32-byte AES-256 key.
//     PBKDF2-HMAC-SHA-256 at exactly 600,000 iterations (the OWASP
//     2023 minimum). The iteration count is part of the format and
//     NOT stored on the wire.
//   - build_aad(...)  → 25-byte associated authenticated data, bound
//     to the v2 SuperBlock's structural fields. See FORMAT-V2.md §3.3.
//   - encrypt_v2 / decrypt_v2 → AES-256-GCM with file-level tag,
//     appended to the ciphertext (so `datasize == ciphertext.len() + 16`).
//
// IV/salt generation lives in the encoder, not here — this module is
// pure crypto and stays test-friendly: feed it explicit salts/IVs to
// pin known-answer vectors. The encoder calls `getrandom` for fresh
// entropy per encode operation.

use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};

use crate::format_v2::{V2_GCM_IV_LEN, V2_GCM_TAG_LEN, V2_KDF_SALT_LEN};

/// PBKDF2 iteration count for v2's KDF. OWASP 2023 minimum for
/// SHA-256-based PBKDF2. Constant: bumping it would be a v3 format
/// break (not stored on the wire). FORMAT-V2.md §3.2.
pub const V2_KDF_ITERATIONS: u32 = 600_000;

/// AES-256 key length, bytes. Output of [`derive_key_v2`].
pub const V2_AES_KEY_LEN: usize = 32;

/// 14-byte AAD prefix that locks the GCM tag to the v2 envelope.
/// Prevents tag collisions with any future format vN that also uses
/// AES-GCM but with different metadata. FORMAT-V2.md §3.3.
pub const V2_AAD_MAGIC: &[u8; 14] = b"ampaper-v2-aad";

/// Total AAD length = 14 (magic) + 1 (feature_flags) + 2 (page_count)
/// + 4 (origsize) + 4 (datasize) = 25 bytes.
pub const V2_AAD_LEN: usize = V2_AAD_MAGIC.len() + 1 + 2 + 4 + 4;
const _: () = assert!(V2_AAD_LEN == 25);

/// Derive a 32-byte AES-256 key from a password and 32-byte salt
/// using PBKDF2-HMAC-SHA-256 at [`V2_KDF_ITERATIONS`].
#[must_use]
pub fn derive_key_v2(password: &[u8], salt: &[u8; V2_KDF_SALT_LEN]) -> [u8; V2_AES_KEY_LEN] {
    let mut key = [0u8; V2_AES_KEY_LEN];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password, salt, V2_KDF_ITERATIONS, &mut key);
    key
}

/// Build the 25-byte AAD that binds the GCM tag to the v2
/// SuperBlock's structural fields. Called identically by encoder
/// (with the values it's about to write) and decoder (with the
/// values it just parsed). Field encoding: little-endian per
/// FORMAT-V2.md §3.3.
#[must_use]
pub fn build_aad(
    feature_flags: u8,
    page_count: u16,
    origsize: u32,
    datasize: u32,
) -> [u8; V2_AAD_LEN] {
    let mut aad = [0u8; V2_AAD_LEN];
    aad[..V2_AAD_MAGIC.len()].copy_from_slice(V2_AAD_MAGIC);
    let mut off = V2_AAD_MAGIC.len();
    aad[off] = feature_flags;
    off += 1;
    aad[off..off + 2].copy_from_slice(&page_count.to_le_bytes());
    off += 2;
    aad[off..off + 4].copy_from_slice(&origsize.to_le_bytes());
    off += 4;
    aad[off..off + 4].copy_from_slice(&datasize.to_le_bytes());
    off += 4;
    debug_assert_eq!(off, V2_AAD_LEN);
    aad
}

/// Encrypt `plaintext` with AES-256-GCM. Returns `ciphertext || tag`
/// (ciphertext.len() + 16 bytes). Caller stores the resulting buffer
/// length as `datasize`.
///
/// Uses RustCrypto's `aead::Aead::encrypt`, which always appends the
/// 128-bit tag to the ciphertext. We don't truncate the tag —
/// FORMAT-V2.md §3.4 / ENCRYPTION-DECISION.md "we explicitly do not
/// truncate auth tags."
pub fn encrypt_v2(
    key: &[u8; V2_AES_KEY_LEN],
    iv: &[u8; V2_GCM_IV_LEN],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, V2CryptoError> {
    let cipher = Aes256Gcm::new(key.into());
    cipher
        .encrypt(
            iv.into(),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| V2CryptoError::EncryptFailed)
}

/// Decrypt `ciphertext_with_tag` (ciphertext + 16-byte tag) with
/// AES-256-GCM, verifying the AAD. Returns the plaintext on success.
///
/// Failure modes:
/// - Wrong key (wrong password) → `Err(InvalidPassword)`. The GCM
///   tag check fails because the derived key disagrees.
/// - Tampered ciphertext → `Err(InvalidPassword)`. (Same path: tag
///   verification fails. The decoder cannot tell tampering from a
///   wrong password apart, which is fine for this use case — both
///   require user action.)
/// - Tampered AAD (e.g., flipped feature_flags bit, mutated
///   page_count) → `Err(InvalidPassword)`.
/// - Buffer shorter than 16 bytes → `Err(CiphertextTooShort)` since
///   the 16-byte tag is mandatory.
pub fn decrypt_v2(
    key: &[u8; V2_AES_KEY_LEN],
    iv: &[u8; V2_GCM_IV_LEN],
    aad: &[u8],
    ciphertext_with_tag: &[u8],
) -> Result<Vec<u8>, V2CryptoError> {
    if ciphertext_with_tag.len() < V2_GCM_TAG_LEN {
        return Err(V2CryptoError::CiphertextTooShort {
            len: ciphertext_with_tag.len(),
        });
    }
    let cipher = Aes256Gcm::new(key.into());
    cipher
        .decrypt(
            iv.into(),
            Payload {
                msg: ciphertext_with_tag,
                aad,
            },
        )
        .map_err(|_| V2CryptoError::InvalidPassword)
}

/// Errors from the v2 crypto path. Distinct from the legacy v1 AES
/// errors — different surface, different recovery story.
#[derive(Debug, PartialEq, Eq)]
pub enum V2CryptoError {
    /// AES-GCM encryption refused the inputs. Practically unreachable
    /// for sane keys/IVs/plaintext sizes; the type system requires a
    /// fallible signature.
    EncryptFailed,
    /// AES-GCM tag verification failed. Indicates wrong password,
    /// tampered ciphertext, or tampered AAD. The decoder surfaces
    /// this as `DecodeError::InvalidPassword` to the caller; users
    /// can retry with a different password.
    InvalidPassword,
    /// Buffer asked to decrypt is shorter than [`V2_GCM_TAG_LEN`] —
    /// no room for the tag. Indicates a corrupt SuperBlock reporting
    /// a too-small `datasize`.
    CiphertextTooShort { len: usize },
}

impl core::fmt::Display for V2CryptoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EncryptFailed => f.write_str("AES-256-GCM encryption rejected"),
            Self::InvalidPassword => f.write_str(
                "AES-256-GCM tag verification failed — wrong password or tampered ciphertext",
            ),
            Self::CiphertextTooShort { len } => write!(
                f,
                "ciphertext buffer is {len} bytes; needs ≥{V2_GCM_TAG_LEN} for the GCM tag"
            ),
        }
    }
}

impl std::error::Error for V2CryptoError {}

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// PBKDF2-HMAC-SHA-256 RFC 6070-style test vector for SHA-256:
    /// P = "password", S = "salt", c = 1, dkLen = 32
    /// DK = 120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b
    /// (Verified against `openssl kdf -keylen 32 -kdfopt digest:SHA256
    ///  -kdfopt pass:password -kdfopt salt:salt -kdfopt iter:1 PBKDF2`.)
    #[test]
    fn pbkdf2_sha256_matches_known_vector_iter_1() {
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(b"password", b"salt", 1, &mut key);
        let expected = hex_to_bytes::<32>(
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b",
        );
        assert_eq!(key, expected);
    }

    /// PBKDF2-HMAC-SHA-256, c=2: known vector from RFC 6070-style
    /// extension to SHA-256:
    /// DK = ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43
    #[test]
    fn pbkdf2_sha256_matches_known_vector_iter_2() {
        let mut key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<sha2::Sha256>(b"password", b"salt", 2, &mut key);
        let expected = hex_to_bytes::<32>(
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43",
        );
        assert_eq!(key, expected);
    }

    /// derive_key_v2 with the spec's parameters runs to completion
    /// and is deterministic — same (password, salt) → same key.
    /// NOT a known-answer test (600k iterations × 32-byte salt would
    /// be a synthetic vector); just pins determinism + stable output.
    #[test]
    fn derive_key_v2_is_deterministic() {
        let salt = [0x42u8; V2_KDF_SALT_LEN];
        let k1 = derive_key_v2(b"correct horse", &salt);
        let k2 = derive_key_v2(b"correct horse", &salt);
        assert_eq!(k1, k2);
        let k3 = derive_key_v2(b"different password", &salt);
        assert_ne!(k1, k3);
        let salt2 = [0x99u8; V2_KDF_SALT_LEN];
        let k4 = derive_key_v2(b"correct horse", &salt2);
        assert_ne!(k1, k4);
    }

    /// AAD construction: byte-for-byte against the spec layout.
    #[test]
    fn build_aad_matches_spec_layout() {
        let aad = build_aad(0xAB, 0x1234, 0x5566_7788, 0x99AA_BBCC);
        // bytes 0..14: "ampaper-v2-aad"
        assert_eq!(&aad[0..14], b"ampaper-v2-aad");
        // byte 14: feature_flags
        assert_eq!(aad[14], 0xAB);
        // bytes 15..17: page_count LE
        assert_eq!(&aad[15..17], &[0x34, 0x12]);
        // bytes 17..21: origsize LE
        assert_eq!(&aad[17..21], &[0x88, 0x77, 0x66, 0x55]);
        // bytes 21..25: datasize LE
        assert_eq!(&aad[21..25], &[0xCC, 0xBB, 0xAA, 0x99]);
    }

    /// AES-256-GCM round-trip: encrypt then decrypt with same key/iv/
    /// AAD recovers the original plaintext byte-for-byte. Pins the
    /// glue without depending on a captured ciphertext.
    #[test]
    fn aes_gcm_round_trip_recovers_plaintext() {
        let key = [0x11u8; V2_AES_KEY_LEN];
        let iv = [0x22u8; V2_GCM_IV_LEN];
        let aad = build_aad(PBM_V2_TEST_FLAGS, 1, 64, 80);
        let plaintext = b"the quick brown fox jumps over the lazy dog";
        let ct = encrypt_v2(&key, &iv, &aad, plaintext).unwrap();
        // GCM appends a 16-byte tag: ciphertext.len() = plaintext + tag.
        assert_eq!(ct.len(), plaintext.len() + V2_GCM_TAG_LEN);
        let pt = decrypt_v2(&key, &iv, &aad, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    /// Wrong password (different key) → InvalidPassword. The 1-bit
    /// difference in the key cascades through GCM enough that the
    /// tag check fails.
    #[test]
    fn aes_gcm_wrong_key_returns_invalid_password() {
        let key = [0x11u8; V2_AES_KEY_LEN];
        let mut wrong_key = key;
        wrong_key[0] ^= 0x01;
        let iv = [0x22u8; V2_GCM_IV_LEN];
        let aad = build_aad(PBM_V2_TEST_FLAGS, 1, 16, 32);
        let ct = encrypt_v2(&key, &iv, &aad, b"sixteen-byte msg").unwrap();
        let err = decrypt_v2(&wrong_key, &iv, &aad, &ct).unwrap_err();
        assert_eq!(err, V2CryptoError::InvalidPassword);
    }

    /// Tampering the AAD (flipping a feature_flags bit) invalidates
    /// the tag — proves the AAD is bound to the ciphertext, which is
    /// the whole point of building it from SuperBlock fields.
    #[test]
    fn aes_gcm_tampered_aad_returns_invalid_password() {
        let key = [0x11u8; V2_AES_KEY_LEN];
        let iv = [0x22u8; V2_GCM_IV_LEN];
        let aad = build_aad(PBM_V2_TEST_FLAGS, 1, 16, 32);
        let ct = encrypt_v2(&key, &iv, &aad, b"sixteen-byte msg").unwrap();
        // Flip the feature_flags byte in the AAD and try to decrypt
        // with the tampered AAD.
        let mut tampered_aad = aad;
        tampered_aad[14] ^= 0x01;
        let err = decrypt_v2(&key, &iv, &tampered_aad, &ct).unwrap_err();
        assert_eq!(err, V2CryptoError::InvalidPassword);
    }

    /// Tampering the ciphertext byte → tag check fails.
    #[test]
    fn aes_gcm_tampered_ciphertext_returns_invalid_password() {
        let key = [0x11u8; V2_AES_KEY_LEN];
        let iv = [0x22u8; V2_GCM_IV_LEN];
        let aad = build_aad(PBM_V2_TEST_FLAGS, 1, 16, 32);
        let mut ct = encrypt_v2(&key, &iv, &aad, b"sixteen-byte msg").unwrap();
        ct[0] ^= 0x01;
        let err = decrypt_v2(&key, &iv, &aad, &ct).unwrap_err();
        assert_eq!(err, V2CryptoError::InvalidPassword);
    }

    /// Empty plaintext → 16-byte ciphertext (just the tag). Pins the
    /// edge case from FORMAT-V2.md §8.
    #[test]
    fn aes_gcm_empty_plaintext_round_trips_to_16_byte_buffer() {
        let key = [0x33u8; V2_AES_KEY_LEN];
        let iv = [0x44u8; V2_GCM_IV_LEN];
        let aad = build_aad(0, 1, 0, V2_GCM_TAG_LEN as u32);
        let ct = encrypt_v2(&key, &iv, &aad, b"").unwrap();
        assert_eq!(ct.len(), V2_GCM_TAG_LEN);
        let pt = decrypt_v2(&key, &iv, &aad, &ct).unwrap();
        assert!(pt.is_empty());
    }

    /// Buffer shorter than the tag → CiphertextTooShort, not a panic.
    #[test]
    fn decrypt_short_buffer_is_specific_error() {
        let key = [0u8; V2_AES_KEY_LEN];
        let iv = [0u8; V2_GCM_IV_LEN];
        let aad = build_aad(0, 1, 0, 0);
        let err = decrypt_v2(&key, &iv, &aad, &[0u8; 8]).unwrap_err();
        assert_eq!(err, V2CryptoError::CiphertextTooShort { len: 8 });
    }

    // --- helpers ------------------------------------------------------------

    /// Constant for AAD-construction tests; not exposed publicly.
    const PBM_V2_TEST_FLAGS: u8 =
        crate::format_v2::PBM_V2_ENCRYPTED | crate::format_v2::PBM_V2_COMPRESSED;

    /// Compile-time hex decode for fixed-length test vectors. Avoids a
    /// dev-dependency on the `hex` crate.
    const fn hex_to_bytes<const N: usize>(s: &str) -> [u8; N] {
        let bytes = s.as_bytes();
        assert!(bytes.len() == 2 * N, "hex string length must be 2*N");
        let mut out = [0u8; N];
        let mut i = 0;
        while i < N {
            out[i] = (hex_digit(bytes[2 * i]) << 4) | hex_digit(bytes[2 * i + 1]);
            i += 1;
        }
        out
    }

    const fn hex_digit(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("invalid hex digit"),
        }
    }
}
