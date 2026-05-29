//! Ed25519 keypair + sign/verify for the audit thesis.
//!
//! The tamper-evident hash chain ([`crate::audit`]) defends against
//! after-the-fact rewrites only if the head hash itself is anchored
//! somewhere an attacker can't reach. Week 4 closes that gap by signing
//! the head with a local Ed25519 key the user controls.
//!
//! ## What lives where
//!
//! - `<data_dir>/audit-key.priv` — 32-byte seed (file permissions 600).
//! - `<data_dir>/audit-key.pub` — 32-byte pubkey, hex-encoded for easy
//!   sharing. This is the file you'd publish on a personal site or pin
//!   in a static config so a verifier later can fetch it.
//! - `<data_dir>/audit-head.sig` — opportunistic file written by the
//!   `audit_chain_sign_head` command. Contains
//!   `{seq, event_hash, ts, signature_hex, pubkey_hex}`.
//!
//! ## Threat model
//!
//! The keypair is local-only. A laptop attacker with full disk access
//! can both rewrite `audit.db` *and* re-sign with the user's key —
//! signatures aren't magical when the signing key sits next to the
//! thing they sign. The value is **portable evidence**: once the signed
//! head leaves the laptop (committed to a public git repo, mailed to a
//! lawyer, anchored to a blockchain), it becomes a fixed point. A later
//! audit can verify the chain against that anchor and detect any
//! divergence.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey, SECRET_KEY_LENGTH};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// Identifier we stamp on signed payloads. Bumping this is how we'd
/// rotate the wire format without confusing old verifiers.
pub const SIGNING_DOMAIN: &str = "palamedes.audit.head.v1";

/// One signed attestation over an audit chain head. The verifier needs
/// `head_payload`, `signature_hex`, and `pubkey_hex` to check; the rest
/// is for humans.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedHead {
    pub seq: i64,
    pub event_hash: String,
    pub ts: String,
    /// Canonical bytes the signature was computed over —
    /// `"{SIGNING_DOMAIN}|{seq}|{event_hash}|{ts}"`.
    pub head_payload: String,
    pub signature_hex: String,
    pub pubkey_hex: String,
}

/// Wrapper around an Ed25519 signing key that knows where its files
/// live on disk.
pub struct KeyPair {
    pub signing: SigningKey,
    pub data_dir: PathBuf,
}

impl KeyPair {
    /// Load the keypair from `<data_dir>/audit-key.{priv,pub}`. If
    /// either file is missing, regenerate a fresh keypair and persist
    /// it.
    ///
    /// The private-key file is written with mode 600 on Unix. On
    /// Windows we trust NTFS user-only ACLs (which `std::fs` honors by
    /// default for files inside the user's profile).
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir).ok();
        let priv_path = data_dir.join("audit-key.priv");
        let pub_path = data_dir.join("audit-key.pub");

        if priv_path.exists() && pub_path.exists() {
            let bytes = std::fs::read(&priv_path)
                .with_context(|| format!("reading {}", priv_path.display()))?;
            if bytes.len() != SECRET_KEY_LENGTH {
                return Err(anyhow!(
                    "audit-key.priv is {} bytes, expected {}",
                    bytes.len(),
                    SECRET_KEY_LENGTH
                ));
            }
            let seed: [u8; SECRET_KEY_LENGTH] = bytes
                .as_slice()
                .try_into()
                .expect("just length-checked");
            let signing = SigningKey::from_bytes(&seed);
            return Ok(Self {
                signing,
                data_dir: data_dir.to_path_buf(),
            });
        }

        // Fresh keypair. ed25519-dalek 2.x's generate() needs a
        // CryptoRng + RngCore; we use OsRng via rand_core to avoid
        // pulling in the full `rand` crate.
        let signing = SigningKey::generate(&mut rand_core::OsRng);
        let seed = signing.to_bytes();
        std::fs::write(&priv_path, seed)
            .with_context(|| format!("writing {}", priv_path.display()))?;
        #[cfg(unix)]
        {
            let mut perms = std::fs::metadata(&priv_path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&priv_path, perms)?;
        }
        let pub_hex = hex::encode(signing.verifying_key().to_bytes());
        std::fs::write(&pub_path, pub_hex.as_bytes())
            .with_context(|| format!("writing {}", pub_path.display()))?;
        Ok(Self {
            signing,
            data_dir: data_dir.to_path_buf(),
        })
    }

    pub fn pubkey_hex(&self) -> String {
        hex::encode(self.signing.verifying_key().to_bytes())
    }

    /// Sign an audit chain head. Returns the full attestation envelope.
    pub fn sign_head(&self, seq: i64, event_hash: &str, ts: &str) -> SignedHead {
        let head_payload = format!("{SIGNING_DOMAIN}|{seq}|{event_hash}|{ts}");
        let sig = self.signing.sign(head_payload.as_bytes());
        SignedHead {
            seq,
            event_hash: event_hash.into(),
            ts: ts.into(),
            head_payload,
            signature_hex: hex::encode(sig.to_bytes()),
            pubkey_hex: self.pubkey_hex(),
        }
    }

    /// Sign an arbitrary message. Used for export-envelope signatures —
    /// the caller passes the canonical JSON bytes.
    pub fn sign_message(&self, message: &[u8]) -> String {
        let sig = self.signing.sign(message);
        hex::encode(sig.to_bytes())
    }

    /// Persist `signed` to `<data_dir>/audit-head.sig` as pretty JSON.
    pub fn write_head_attestation(&self, signed: &SignedHead) -> Result<PathBuf> {
        let path = self.data_dir.join("audit-head.sig");
        let body = serde_json::to_string_pretty(signed)?;
        std::fs::write(&path, body.as_bytes())?;
        Ok(path)
    }
}

/// Verify a [`SignedHead`] envelope against a pinned pubkey. Returns
/// `Ok(())` if the signature is good; `Err` if the signature is bad,
/// the pubkey doesn't match, or any hex decoding fails.
pub fn verify_signed_head(signed: &SignedHead, expected_pubkey_hex: &str) -> Result<()> {
    if signed.pubkey_hex.eq_ignore_ascii_case(expected_pubkey_hex) {
        verify_message(
            &signed.head_payload,
            &signed.signature_hex,
            &signed.pubkey_hex,
        )
    } else {
        Err(anyhow!(
            "signed-head pubkey {} does not match expected {}",
            &signed.pubkey_hex[..16.min(signed.pubkey_hex.len())],
            &expected_pubkey_hex[..16.min(expected_pubkey_hex.len())]
        ))
    }
}

/// Verify a raw (message, signature, pubkey) triple.
pub fn verify_message(message: &str, signature_hex: &str, pubkey_hex: &str) -> Result<()> {
    let pubkey_bytes: [u8; 32] = hex::decode(pubkey_hex)
        .context("pubkey is not valid hex")?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("pubkey must be 32 bytes (64 hex chars)"))?;
    let sig_bytes: [u8; 64] = hex::decode(signature_hex)
        .context("signature is not valid hex")?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("signature must be 64 bytes (128 hex chars)"))?;
    let verifying = VerifyingKey::from_bytes(&pubkey_bytes)
        .context("pubkey is not a valid Ed25519 point")?;
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    verifying
        .verify(message.as_bytes(), &sig)
        .map_err(|e| anyhow!("signature verification failed: {e}"))
}

// Lightweight hex helpers — avoid pulling in the full `hex` crate for
// one use site. Encode is lowercase, decode is case-insensitive.
mod hex {
    use anyhow::{anyhow, Result};

    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let bytes = bytes.as_ref();
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0x0f) as usize] as char);
        }
        s
    }

    pub fn decode(s: impl AsRef<str>) -> Result<Vec<u8>> {
        let s = s.as_ref();
        if s.len() % 2 != 0 {
            return Err(anyhow!("hex string has odd length"));
        }
        let mut out = Vec::with_capacity(s.len() / 2);
        let bytes = s.as_bytes();
        for pair in bytes.chunks_exact(2) {
            let hi = nib(pair[0])?;
            let lo = nib(pair[1])?;
            out.push((hi << 4) | lo);
        }
        Ok(out)
    }

    fn nib(c: u8) -> Result<u8> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(10 + c - b'a'),
            b'A'..=b'F' => Ok(10 + c - b'A'),
            _ => Err(anyhow!("invalid hex char: {:?}", c as char)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_or_create_generates_keypair_and_persists_files() {
        let tmp = TempDir::new().unwrap();
        let kp = KeyPair::load_or_create(tmp.path()).unwrap();
        let priv_path = tmp.path().join("audit-key.priv");
        let pub_path = tmp.path().join("audit-key.pub");
        assert!(priv_path.exists());
        assert!(pub_path.exists());
        let priv_bytes = std::fs::read(&priv_path).unwrap();
        assert_eq!(priv_bytes.len(), SECRET_KEY_LENGTH);
        let pub_hex = std::fs::read_to_string(&pub_path).unwrap();
        assert_eq!(pub_hex, kp.pubkey_hex());
    }

    #[test]
    fn load_or_create_is_idempotent_across_calls() {
        let tmp = TempDir::new().unwrap();
        let kp1 = KeyPair::load_or_create(tmp.path()).unwrap();
        let kp2 = KeyPair::load_or_create(tmp.path()).unwrap();
        // Same key persisted across calls — pubkeys must match.
        assert_eq!(kp1.pubkey_hex(), kp2.pubkey_hex());
    }

    #[cfg(unix)]
    #[test]
    fn private_key_is_mode_600_on_unix() {
        let tmp = TempDir::new().unwrap();
        KeyPair::load_or_create(tmp.path()).unwrap();
        let path = tmp.path().join("audit-key.priv");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn sign_head_round_trips_through_verify() {
        let tmp = TempDir::new().unwrap();
        let kp = KeyPair::load_or_create(tmp.path()).unwrap();
        let signed = kp.sign_head(42, "deadbeef", "2026-05-29T12:00:00Z");
        verify_signed_head(&signed, &kp.pubkey_hex()).unwrap();
    }

    #[test]
    fn verify_rejects_tampered_event_hash() {
        let tmp = TempDir::new().unwrap();
        let kp = KeyPair::load_or_create(tmp.path()).unwrap();
        let mut signed = kp.sign_head(42, "deadbeef", "2026-05-29T12:00:00Z");
        // Attacker rewrites the event_hash but can't re-sign.
        signed.event_hash = "cafef00d".into();
        signed.head_payload = format!(
            "{SIGNING_DOMAIN}|{}|{}|{}",
            signed.seq, signed.event_hash, signed.ts
        );
        let err = verify_signed_head(&signed, &kp.pubkey_hex());
        assert!(err.is_err(), "tampered payload must fail verification");
    }

    #[test]
    fn verify_rejects_wrong_pubkey() {
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();
        let kp_a = KeyPair::load_or_create(tmp_a.path()).unwrap();
        let kp_b = KeyPair::load_or_create(tmp_b.path()).unwrap();
        let signed = kp_a.sign_head(1, "abc", "ts");
        let err = verify_signed_head(&signed, &kp_b.pubkey_hex());
        assert!(err.is_err(), "wrong pubkey must fail verification");
    }

    #[test]
    fn write_head_attestation_writes_round_trippable_json() {
        let tmp = TempDir::new().unwrap();
        let kp = KeyPair::load_or_create(tmp.path()).unwrap();
        let signed = kp.sign_head(7, "abc123", "2026-01-01T00:00:00Z");
        let path = kp.write_head_attestation(&signed).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let parsed: SignedHead = serde_json::from_str(&body).unwrap();
        verify_signed_head(&parsed, &kp.pubkey_hex()).unwrap();
    }

    #[test]
    fn sign_message_round_trips() {
        let tmp = TempDir::new().unwrap();
        let kp = KeyPair::load_or_create(tmp.path()).unwrap();
        let msg = b"hello palamedes";
        let sig = kp.sign_message(msg);
        verify_message(
            std::str::from_utf8(msg).unwrap(),
            &sig,
            &kp.pubkey_hex(),
        )
        .unwrap();
    }

    #[test]
    fn verify_rejects_corrupt_hex() {
        let tmp = TempDir::new().unwrap();
        let kp = KeyPair::load_or_create(tmp.path()).unwrap();
        let err = verify_message("x", "not-hex", &kp.pubkey_hex());
        assert!(err.is_err());
    }
}
