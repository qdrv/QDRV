// Author: Michael Lauzon <qdrv2026@gmail.com>
// SPDX-License-Identifier: GPL-2.0-only
//! Deterministic metadata manifest hashing and signing helpers.
//!
//! Signatures use HMAC-SHA256 (RFC 2104). The previous `"sha256-keyed"`
//! construction — `SHA256(key || payload_hash)` — is rejected at verify time
//! because it is not a length-extension-resistant MAC; any manifest with the
//! legacy algorithm label must be re-signed against this module.

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

/// Current signature algorithm label written by [`sign_manifest`].
const SIGNATURE_ALGORITHM_HMAC_SHA256: &str = "hmac-sha256";
/// Legacy algorithm label intentionally rejected on verify.
const LEGACY_SIGNATURE_ALGORITHM_KEYED: &str = "sha256-keyed";

/// Upper bound on the byte length of a `signer` identifier. Real signers
/// are short labels (tool name, vendor handle); 1 KiB is generous enough
/// that legitimate identifiers fit but stops an adversarial manifest with
/// a multi-gigabyte `signer` string from causing memory pressure or the
/// `u32::try_from` panic that `hmac_signature` would otherwise produce
/// (P3-1 follow-up).
const MAX_SIGNER_BYTES: usize = 1024;

/// Upper bound on the algorithm-label string fields
/// (`payload_hash_algorithm`, `signature_algorithm`). The current accepted
/// values are 6 and 11 characters respectively; 32 bytes leaves headroom
/// for future algorithm names while still capping adversarial input
/// (P5-4).
const MAX_ALGORITHM_LABEL_BYTES: usize = 32;

/// Exact length of a SHA-256 hex digest (32 bytes × 2 hex chars). Both
/// `payload_hash_hex` and `signature_hex` (HMAC-SHA256 output) must be
/// this length. Enforcing it pre-empts the constant-time-`ct_eq` length
/// short-circuit so we never call the comparator with multi-gigabyte
/// strings that serde happily allocates during deserialisation (P5-4).
const SHA256_HEX_LEN: usize = 64;

/// Error returned by `sign_manifest` / `verify_manifest`.
///
/// Returned as a borrowed `&'static str` to keep the error-handling
/// surface minimal — every variant maps to a fixed, descriptive message
/// suitable for direct logging.
pub type ManifestError = &'static str;

/// Signed manifest over a metadata payload hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedMetadataManifest {
    /// Manifest schema version.
    pub schema_version: u16,
    /// Hash algorithm.
    pub payload_hash_algorithm: String,
    /// Hex-encoded payload hash.
    pub payload_hash_hex: String,
    /// Signature algorithm string.
    pub signature_algorithm: String,
    /// Hex-encoded deterministic signature.
    pub signature_hex: String,
    /// Free-form signer identifier.
    pub signer: String,
}

/// Computes a deterministic SHA-256 digest hex string.
///
/// # Example
///
/// ```
/// use qdrv_meta::sha256_hex;
///
/// // Golden vector also used by the in-crate
/// // `golden_payload_hash_vector_stable` regression test.
/// let payload = br#"{"demo":"metadata"}"#;
/// assert_eq!(
///     sha256_hex(payload),
///     "0759e53b689acf6160a2118b04785abf4c2fe8e74f433db702ceb1a1afaf8323"
/// );
/// // The empty input hashes to the canonical SHA-256 of zero bytes.
/// assert_eq!(
///     sha256_hex(b""),
///     "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
/// );
/// ```
pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    bytes_to_hex(&digest)
}

/// Builds and signs a manifest for payload bytes using HMAC-SHA256.
///
/// `signer` is bound into the MAC input alongside the payload hash so two
/// manifests over the same payload but different signers are not
/// interchangeable.
///
/// # Errors
/// Returns `"signer identifier too long"` if `signer.len() > MAX_SIGNER_BYTES`.
/// The cap exists so adversarial multi-gigabyte signer strings cannot
/// drive the HMAC length-prefix encoding past `u32::MAX` (P3-1).
pub fn sign_manifest(
    payload: &[u8],
    signer: &str,
    signing_key: &[u8],
) -> Result<SignedMetadataManifest, ManifestError> {
    if signer.len() > MAX_SIGNER_BYTES {
        return Err("signer identifier too long");
    }
    let payload_hash = Sha256::digest(payload);
    let signature = hmac_signature(&payload_hash, signer, signing_key);
    Ok(SignedMetadataManifest {
        schema_version: 1,
        payload_hash_algorithm: "sha256".to_string(),
        payload_hash_hex: bytes_to_hex(&payload_hash),
        signature_algorithm: SIGNATURE_ALGORITHM_HMAC_SHA256.to_string(),
        signature_hex: bytes_to_hex(&signature),
        signer: signer.to_string(),
    })
}

/// Verifies payload hash and HMAC-SHA256 signature in constant time.
pub fn verify_manifest(
    payload: &[u8],
    manifest: &SignedMetadataManifest,
    signing_key: &[u8],
) -> Result<(), ManifestError> {
    // P5-4: bound every string-shaped field up front so a manifest with
    // multi-gigabyte fields cannot push our verifier past the trivial
    // string comparisons below. Serde has already allocated the strings
    // by the time we get here, but capping them tells callers reading
    // the error message what the actual constraint was.
    if manifest.payload_hash_algorithm.len() > MAX_ALGORITHM_LABEL_BYTES {
        return Err("payload_hash_algorithm label too long");
    }
    if manifest.signature_algorithm.len() > MAX_ALGORITHM_LABEL_BYTES {
        return Err("signature_algorithm label too long");
    }
    if manifest.payload_hash_hex.len() != SHA256_HEX_LEN {
        return Err("payload_hash_hex must be 64 hex characters");
    }
    if manifest.signature_hex.len() != SHA256_HEX_LEN {
        return Err("signature_hex must be 64 hex characters");
    }
    if manifest.payload_hash_algorithm != "sha256" {
        return Err("unsupported payload_hash_algorithm");
    }
    if manifest.signature_algorithm == LEGACY_SIGNATURE_ALGORITHM_KEYED {
        return Err("legacy sha256-keyed signature is not accepted; re-sign with hmac-sha256");
    }
    if manifest.signature_algorithm != SIGNATURE_ALGORITHM_HMAC_SHA256 {
        return Err("unsupported signature_algorithm");
    }
    // Reject pathological signer strings before they reach `hmac_signature`;
    // the function bound-checks the same way but failing here gives the
    // verifier a chance to short-circuit before doing any hashing work.
    if manifest.signer.len() > MAX_SIGNER_BYTES {
        return Err("signer identifier too long");
    }

    let payload_hash = Sha256::digest(payload);
    let actual_payload_hash_hex = bytes_to_hex(&payload_hash);
    // Constant-time string comparison: the hash string itself is not secret,
    // but matching this path's style with the signature check keeps timing
    // characteristics uniform across error branches.
    if actual_payload_hash_hex
        .as_bytes()
        .ct_eq(manifest.payload_hash_hex.as_bytes())
        .unwrap_u8()
        == 0
    {
        return Err("payload hash mismatch");
    }

    let expected_signature_bytes = hmac_signature(&payload_hash, &manifest.signer, signing_key);
    let expected_hex = bytes_to_hex(&expected_signature_bytes);
    if expected_hex
        .as_bytes()
        .ct_eq(manifest.signature_hex.as_bytes())
        .unwrap_u8()
        == 0
    {
        return Err("manifest signature mismatch");
    }
    Ok(())
}

/// Internal HMAC helper. Callers MUST have already validated
/// `signer.len() <= MAX_SIGNER_BYTES`, which guarantees the
/// `u32::try_from` here cannot fail (1 KiB fits comfortably in u32).
///
/// `HmacSha256::new_from_slice` is documented as infallible for any key
/// length, including empty (RFC 2104 allows it).
fn hmac_signature(payload_hash: &[u8], signer: &str, signing_key: &[u8]) -> Vec<u8> {
    debug_assert!(
        signer.len() <= MAX_SIGNER_BYTES,
        "hmac_signature called with un-bounded signer; sign/verify must enforce cap first"
    );
    // `Hmac::new_from_slice` only returns `Err(InvalidLength)` for ciphers
    // that impose a minimum key length. SHA-256 does not, so for any byte
    // slice (including empty, per RFC 2104) this is a compile-time-fixed
    // success. The expect message documents that contract and the workspace
    // `expect_used` lint is suppressed here because there is no recoverable
    // error path: a future breaking change in the `hmac` crate would surface
    // immediately in the unit tests.
    #[allow(clippy::expect_used)]
    let mut mac = HmacSha256::new_from_slice(signing_key)
        .expect("HmacSha256::new_from_slice is infallible for any key length");
    // Domain-separate the signer identifier from the payload hash so a
    // longer/shorter `signer` cannot shift the boundary.
    mac.update(b"qdrv-manifest-v1\0");
    // The cap above keeps signer.len() well within u32; truncation here
    // would be a bug in the caller, hence the saturating fall-back rather
    // than a panic.
    let signer_len: u32 = u32::try_from(signer.len()).unwrap_or(u32::MAX);
    mac.update(&signer_len.to_be_bytes());
    mac.update(signer.as_bytes());
    mac.update(payload_hash);
    mac.finalize().into_bytes().to_vec()
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_manifest_roundtrip() {
        let payload = br#"{"demo":"metadata"}"#;
        let key = b"qdrv-conformance-key";
        let manifest = sign_manifest(payload, "unit-test", key).unwrap();
        assert!(verify_manifest(payload, &manifest, key).is_ok());
    }

    #[test]
    fn verify_detects_tampered_payload() {
        let payload = br#"{"demo":"metadata"}"#;
        let key = b"qdrv-conformance-key";
        let manifest = sign_manifest(payload, "unit-test", key).unwrap();
        let tampered = br#"{"demo":"changed"}"#;
        assert!(verify_manifest(tampered, &manifest, key).is_err());
    }

    #[test]
    fn golden_payload_hash_vector_stable() {
        let payload = br#"{"demo":"metadata"}"#;
        let expected = "0759e53b689acf6160a2118b04785abf4c2fe8e74f433db702ceb1a1afaf8323";
        assert_eq!(sha256_hex(payload), expected);
    }

    #[test]
    fn verify_rejects_legacy_sha256_keyed_label() {
        let payload = br#"{"demo":"metadata"}"#;
        let key = b"qdrv-conformance-key";
        let mut manifest = sign_manifest(payload, "unit-test", key).unwrap();
        manifest.signature_algorithm = "sha256-keyed".to_string();
        let err = verify_manifest(payload, &manifest, key).unwrap_err();
        assert!(
            err.contains("legacy sha256-keyed"),
            "expected legacy rejection, got: {err}"
        );
    }

    #[test]
    fn verify_rejects_signer_swap() {
        let payload = br#"{"demo":"metadata"}"#;
        let key = b"qdrv-conformance-key";
        let mut manifest = sign_manifest(payload, "signer-a", key).unwrap();
        manifest.signer = "signer-b".to_string();
        assert!(verify_manifest(payload, &manifest, key).is_err());
    }

    #[test]
    fn current_signature_algorithm_is_hmac_sha256() {
        let payload = br#"{}"#;
        let m = sign_manifest(payload, "x", b"k").unwrap();
        assert_eq!(m.signature_algorithm, "hmac-sha256");
    }

    /// P3-1 regression: sign_manifest must reject a signer string larger
    /// than the configured cap, so an adversarial caller cannot push the
    /// internal `u32` length encoding past its limit.
    #[test]
    fn sign_manifest_rejects_oversized_signer() {
        let payload = br#"{}"#;
        let huge_signer = "a".repeat(MAX_SIGNER_BYTES + 1);
        let err = sign_manifest(payload, &huge_signer, b"k").unwrap_err();
        assert_eq!(err, "signer identifier too long");
    }

    /// P3-1 regression: verify_manifest must also reject an oversized
    /// signer field arriving in a deserialised manifest, before the HMAC
    /// pipeline tries to length-encode it.
    #[test]
    fn verify_manifest_rejects_oversized_signer_field() {
        let payload = br#"{}"#;
        let key = b"k";
        let mut manifest = sign_manifest(payload, "ok", key).unwrap();
        manifest.signer = "x".repeat(MAX_SIGNER_BYTES + 1);
        let err = verify_manifest(payload, &manifest, key).unwrap_err();
        assert_eq!(err, "signer identifier too long");
    }

    /// P5-4 regression: every string-shaped manifest field has an upper
    /// bound checked before the verifier touches the value, so a
    /// multi-megabyte garbage field cannot drive comparator work past the
    /// trivial-rejection fast path.
    #[test]
    fn verify_manifest_rejects_oversized_string_fields() {
        let payload = br#"{}"#;
        let key = b"k";
        let base = sign_manifest(payload, "ok", key).unwrap();

        let huge = "x".repeat(MAX_ALGORITHM_LABEL_BYTES + 1);
        let huge_hex = "a".repeat(SHA256_HEX_LEN + 1);

        let mut m = base.clone();
        m.payload_hash_algorithm = huge.clone();
        assert_eq!(
            verify_manifest(payload, &m, key).unwrap_err(),
            "payload_hash_algorithm label too long"
        );

        let mut m = base.clone();
        m.signature_algorithm = huge.clone();
        assert_eq!(
            verify_manifest(payload, &m, key).unwrap_err(),
            "signature_algorithm label too long"
        );

        let mut m = base.clone();
        m.payload_hash_hex = huge_hex.clone();
        assert_eq!(
            verify_manifest(payload, &m, key).unwrap_err(),
            "payload_hash_hex must be 64 hex characters"
        );

        let mut m = base.clone();
        m.signature_hex = huge_hex;
        assert_eq!(
            verify_manifest(payload, &m, key).unwrap_err(),
            "signature_hex must be 64 hex characters"
        );

        // Short-length variants of the hex fields are also rejected.
        let mut m = base.clone();
        m.payload_hash_hex = "abc".to_string();
        assert_eq!(
            verify_manifest(payload, &m, key).unwrap_err(),
            "payload_hash_hex must be 64 hex characters"
        );
    }
}
