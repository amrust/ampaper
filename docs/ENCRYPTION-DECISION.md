# Encryption — legacy AES-192 read, AES-256-GCM forward

Decision record. The user's ask: read-only AES-192 for compatibility with PaperBack 1.10 prints, plus a new modern cipher chosen for the ampaper-v2 format. This file walks through the choice.

## Constraints

- **Backward compatibility is non-negotiable for read.** Existing PaperBack 1.10 AES-192 prints must decode in ampaper. mrpods removing AES is a regression we explicitly reject.
- **Backward compatibility is rejected for write.** AES-192 is not broken, but the way 1.10 uses it is: hand-rolled CBC + key stretching, no AEAD, MAC-by-CRC at most. Re-emitting that today would be cargo-culting.
- **Archival-grade.** Paper backups outlive their software. The cipher we pick now must be defensible in 2050. That favors well-vetted, conservative choices.
- **Embedded-friendly.** Whatever we pick has to fit alongside RS + bzip2 in the per-block budget without eating density.

## The candidates for the v2 forward mode

| Option | Pros | Cons |
|---|---|---|
| **AES-256-GCM** | Standard AEAD; widely audited; hardware acceleration on every modern CPU; 96-bit IV / 128-bit tag fits cleanly into a per-block envelope. | Catastrophic IV reuse (so the design has to make IV reuse structurally impossible). |
| **AES-256-SIV (RFC 5297)** | Misuse-resistant: deterministic, IV reuse degrades to "leaks equality" rather than full break. | More CPU; 256-bit auth tag eats more density; less ubiquitous. |
| **ChaCha20-Poly1305** | Constant-time on platforms without AES-NI; Poly1305 MAC is well understood. | "AES-NI everywhere" makes the speed argument moot on x64 / ARM64; less obvious choice for an archival format reviewers expect to see AES on. |
| **XChaCha20-Poly1305** | 192-bit nonce → IV-reuse risk effectively zero with random nonces. | Same speed argument; less standard. |

## Recommendation: AES-256-GCM

- AES-NI exists on every Windows-supported x64 CPU since 2010 and every ARM64 with AES extension. Speed isn't a constraint.
- GCM is a NIST-approved AEAD; reviewers and auditors recognize it instantly.
- A 96-bit IV is small enough not to hurt our per-page byte budget.
- Tag length: 128-bit (full GCM tag), no truncation.

**GCM granularity decision (2026-05-01): file-level, not per-block.** The original draft of this document specified per-block GCM with IVs derived as `SHA-256(page_index || block_index || file_random_salt)[:12]`. That bakes encryption INTO the block layer and costs ~18% of the data area per block (the 16-byte tag eats into NDATA). For a future where M12 ships color encoding (CMYK/RGB sub-channels giving 3-4× density) plus adaptive (n, k) RS, per-block GCM compounds the crypto tax with every density gain — a 4-color page loses 16 bytes per block per plane. File-level GCM puts encryption ABOVE the block layer; M12 changes are layer-orthogonal.

The only real argument for per-block GCM is per-block cryptographic authenticity vs CRC's malleability. That matters for an adversarial network channel, not for a paper archive: realistic threats are scan errors (RS handles per-block) and whole-file tampering (file-level GCM tag detects). Selective per-block tampering on a printed page is contrived.

**IV scheme.** Single 96-bit IV per encode operation, generated fresh from `getrandom`. Stored in the v2 SuperBlock. With file-level GCM and a per-encode-operation IV, IV reuse is structurally impossible across encodes. Per-block IV derivation isn't needed when there's only one encryption operation.

**Tag placement.** The 16-byte GCM tag is appended to the ciphertext (not stored in the SuperBlock header). `datasize` in the v2 SuperBlock includes the tag length, so decoders that recover `datasize` bytes have the tag automatically.

**AAD construction.** The Associated Authenticated Data passed to GCM includes:
- v2 magic bytes (`b"ampaper-v2"` or equivalent)
- `page_count` (u32, total pages in this encode)
- `origsize` (u32, plaintext byte count)
- `datasize` (u32, ciphertext + tag byte count)

This binds the tag to structural metadata: an attacker cannot truncate pages, change reported sizes, or splice files without tag mismatch.

## KDF for the v2 forward mode

- **PBKDF2-HMAC-SHA-256, 600,000 iterations** — OWASP 2023 minimum for SHA-256 PBKDF2.
- 256-bit salt, per-encode-operation, fresh from `getrandom`. Stored in the v2 SuperBlock.
- Output: 256-bit AES key. (The GCM IV is independent random data, not derived from the password.)

Argon2id would be stronger but adds a dependency + tunable parameters that scare archivists ("will this still verify in 30 years if the parameters change?"). PBKDF2-HMAC-SHA-256 with a high iteration count is the conservative archival choice.

## Legacy AES-192 read path (M7)

Strict read-only. The encoder refuses to emit v1-AES-192. The decoder accepts it but logs a one-time warning that the format is deprecated. The implementation in ampaper:

1. Mirrors PaperBack 1.10's exact KDF (TBD — pin in M7 once `paperbak-1.10` source is read; document in `FORMAT-V1.md`).
2. AES-192-CBC, no AEAD; integrity falls back to the per-block CRC-16 already in the format. (This is a security weakness vs. v2; users with sensitive data should re-encode to v2.)
3. Lives behind `#[deprecated]` markers and a `legacy_v1_decode` feature gate so callers acknowledge the security posture.

## What we explicitly do not do

- **Roll our own KDF.** PBKDF2 has been baking for 25 years; we use it.
- **Truncate auth tags.** Saving 8 bytes is not worth weakening authentication.
- **Encrypt-then-MAC with separate primitives.** AEAD is the modern answer; we use AEAD.
- **Make AES-192 the default for v2.** It's not broken, but it has weaker key-search margin than AES-256, and it telegraphs "we're trying to look like 1.10" rather than "we made a clean break."
- **Add a quantum-resistant cipher.** Symmetric AES-256 has 128-bit post-quantum security via Grover's algorithm — comfortable for archival timeframes. Adding a PQ-KEM here would be over-engineered.

## Open question for the user

The ask was "AES-192 read, AES-256 write." Confirming the choices above:

- AES-256-GCM (authenticated) over AES-256-CBC (not authenticated).
- PBKDF2-HMAC-SHA-256, 600k iterations.
- Per-block IV derived from `(page, block, salt)` so reuse is structurally impossible.

If you want Argon2id KDF or XChaCha20-Poly1305 instead, this is the place to push back — the rest of the design depends on what we land here.
