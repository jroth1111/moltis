//! Device pairing state machine and device token management.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD},
};
use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("pair request not found")]
    PairRequestNotFound,

    #[error("pair request already {0:?}")]
    PairRequestNotPending(PairStatus),

    #[error("pair request expired")]
    PairRequestExpired,

    #[error("pair request not verified")]
    PairRequestNotVerified,

    #[error("pair request missing public key")]
    PairRequestMissingPublicKey,

    #[error("invalid pair proof")]
    PairRequestInvalidProof,

    #[error("device not found")]
    DeviceNotFound,
}

pub type Result<T> = std::result::Result<T, Error>;

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PairStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

#[derive(Debug, Clone)]
pub struct PairRequest {
    pub id: String,
    pub device_id: String,
    pub display_name: Option<String>,
    pub platform: String,
    pub public_key: Option<String>,
    pub nonce: String,
    pub verified: bool,
    pub status: PairStatus,
    pub created_at: Instant,
    pub expires_at: Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceToken {
    pub token: String,
    pub device_id: String,
    pub scopes: Vec<String>,
    pub issued_at_ms: u64,
    pub revoked: bool,
}

// ── Pairing state ───────────────────────────────────────────────────────────

/// In-memory pairing state; tracks pending pair requests and issued device tokens.
pub struct PairingState {
    pending: HashMap<String, PairRequest>,
    devices: HashMap<String, DeviceToken>,
    pair_ttl: Duration,
}

impl Default for PairingState {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingState {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            devices: HashMap::new(),
            pair_ttl: Duration::from_secs(300), // 5 min
        }
    }

    /// Submit a new pairing request. Returns the generated nonce.
    pub fn request_pair(
        &mut self,
        device_id: &str,
        display_name: Option<&str>,
        platform: &str,
        public_key: Option<&str>,
    ) -> PairRequest {
        let id = uuid::Uuid::new_v4().to_string();
        let nonce = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        let req = PairRequest {
            id: id.clone(),
            device_id: device_id.to_string(),
            display_name: display_name.map(|s| s.to_string()),
            platform: platform.to_string(),
            public_key: public_key.map(|s| s.to_string()),
            nonce,
            verified: false,
            status: PairStatus::Pending,
            created_at: now,
            expires_at: now + self.pair_ttl,
        };
        self.pending.insert(id, req.clone());
        req
    }

    /// List all non-expired pending requests.
    pub fn list_pending(&self) -> Vec<&PairRequest> {
        let now = Instant::now();
        self.pending
            .values()
            .filter(|r| r.status == PairStatus::Pending && now < r.expires_at)
            .collect()
    }

    /// Approve a pending pair request. Issues a device token.
    pub fn approve(&mut self, pair_id: &str) -> Result<DeviceToken> {
        let req = self
            .pending
            .get_mut(pair_id)
            .ok_or(Error::PairRequestNotFound)?;
        if req.status != PairStatus::Pending {
            return Err(Error::PairRequestNotPending(req.status));
        }
        if Instant::now() > req.expires_at {
            req.status = PairStatus::Expired;
            return Err(Error::PairRequestExpired);
        }
        if !req.verified {
            return Err(Error::PairRequestNotVerified);
        }
        req.status = PairStatus::Approved;

        let token = DeviceToken {
            token: uuid::Uuid::new_v4().to_string(),
            device_id: req.device_id.clone(),
            scopes: vec![
                "operator.read".into(),
                "operator.write".into(),
                "operator.approvals".into(),
            ],
            issued_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            revoked: false,
        };
        self.devices.insert(req.device_id.clone(), token.clone());
        Ok(token)
    }

    /// Verify a pending pairing request using a signature bound to its challenge nonce.
    ///
    /// Accepted encodings:
    /// - `public_key`: base64/base64url SEC1-encoded P-256 key (compressed or uncompressed).
    /// - `signature`: base64/base64url ECDSA signature in DER or 64-byte `r || s` format.
    pub fn verify(&mut self, pair_id: &str, signature: &str) -> Result<()> {
        let req = self
            .pending
            .get_mut(pair_id)
            .ok_or(Error::PairRequestNotFound)?;
        if req.status != PairStatus::Pending {
            return Err(Error::PairRequestNotPending(req.status));
        }
        if Instant::now() > req.expires_at {
            req.status = PairStatus::Expired;
            return Err(Error::PairRequestExpired);
        }

        let public_key = req
            .public_key
            .as_deref()
            .ok_or(Error::PairRequestMissingPublicKey)?;
        verify_pair_proof(public_key, pair_id, &req.nonce, signature)?;
        req.verified = true;
        Ok(())
    }

    /// Reject a pending pair request.
    pub fn reject(&mut self, pair_id: &str) -> Result<()> {
        let req = self
            .pending
            .get_mut(pair_id)
            .ok_or(Error::PairRequestNotFound)?;
        if req.status != PairStatus::Pending {
            return Err(Error::PairRequestNotPending(req.status));
        }
        req.status = PairStatus::Rejected;
        Ok(())
    }

    /// List all approved (non-revoked) devices.
    pub fn list_devices(&self) -> Vec<&DeviceToken> {
        self.devices.values().filter(|d| !d.revoked).collect()
    }

    /// Rotate a device token: revoke old, issue new.
    pub fn rotate_token(&mut self, device_id: &str) -> Result<DeviceToken> {
        let existing = self
            .devices
            .get_mut(device_id)
            .ok_or(Error::DeviceNotFound)?;
        existing.revoked = true;

        let new_token = DeviceToken {
            token: uuid::Uuid::new_v4().to_string(),
            device_id: device_id.to_string(),
            scopes: existing.scopes.clone(),
            issued_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            revoked: false,
        };
        self.devices
            .insert(device_id.to_string(), new_token.clone());
        Ok(new_token)
    }

    /// Revoke a device token permanently.
    pub fn revoke_token(&mut self, device_id: &str) -> Result<()> {
        let existing = self
            .devices
            .get_mut(device_id)
            .ok_or(Error::DeviceNotFound)?;
        existing.revoked = true;
        Ok(())
    }

    /// Evict expired pending requests.
    pub fn evict_expired(&mut self) {
        let now = Instant::now();
        self.pending
            .retain(|_, r| !(r.status == PairStatus::Pending && now > r.expires_at));
    }
}

fn pairing_proof_transcript(pair_id: &str, nonce: &str) -> String {
    format!("moltis-pairing-v1\n{pair_id}\n{nonce}")
}

fn verify_pair_proof(public_key: &str, pair_id: &str, nonce: &str, signature: &str) -> Result<()> {
    let key_bytes = decode_base64(public_key).map_err(|_| Error::PairRequestInvalidProof)?;
    let signature_bytes = decode_base64(signature).map_err(|_| Error::PairRequestInvalidProof)?;
    let verifying_key =
        VerifyingKey::from_sec1_bytes(&key_bytes).map_err(|_| Error::PairRequestInvalidProof)?;
    let signature = Signature::from_der(&signature_bytes)
        .or_else(|_| Signature::from_slice(&signature_bytes))
        .map_err(|_| Error::PairRequestInvalidProof)?;

    let transcript = pairing_proof_transcript(pair_id, nonce);
    verifying_key
        .verify(transcript.as_bytes(), &signature)
        .map_err(|_| Error::PairRequestInvalidProof)
}

fn decode_base64(value: &str) -> std::result::Result<Vec<u8>, base64::DecodeError> {
    let value = value.trim();
    if let Ok(v) = URL_SAFE_NO_PAD.decode(value) {
        tracing::debug!(variant = "url-safe-no-pad", "decoded base64 value");
        return Ok(v);
    }
    if let Ok(v) = URL_SAFE.decode(value) {
        tracing::debug!(variant = "url-safe", "decoded base64 value");
        return Ok(v);
    }
    if let Ok(v) = STANDARD_NO_PAD.decode(value) {
        tracing::debug!(variant = "standard-no-pad", "decoded base64 value");
        return Ok(v);
    }
    let v = STANDARD.decode(value)?;
    tracing::debug!(variant = "standard", "decoded base64 value");
    Ok(v)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    use p256::ecdsa::{SigningKey, signature::Signer};
    use p256::elliptic_curve::rand_core::OsRng;

    fn request_with_signing_key(state: &mut PairingState, signing_key: &SigningKey) -> PairRequest {
        let public_key = signing_key.verifying_key().to_encoded_point(false);
        let public_key_b64 = URL_SAFE_NO_PAD.encode(public_key.as_bytes());
        state.request_pair("ios-device-1", Some("iPhone"), "ios", Some(&public_key_b64))
    }

    fn sign_pair_proof(signing_key: &SigningKey, pair_id: &str, nonce: &str) -> String {
        let transcript = pairing_proof_transcript(pair_id, nonce);
        let signature: Signature = signing_key.sign(transcript.as_bytes());
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    }

    #[test]
    fn verify_then_approve_succeeds() {
        let mut state = PairingState::new();
        let signing_key = SigningKey::random(&mut OsRng);
        let request = request_with_signing_key(&mut state, &signing_key);
        let signature = sign_pair_proof(&signing_key, &request.id, &request.nonce);

        state.verify(&request.id, &signature).expect("verify");
        let token = state.approve(&request.id).expect("approve");

        assert_eq!(token.device_id, request.device_id);
    }

    #[test]
    fn verify_rejects_invalid_signature() {
        let mut state = PairingState::new();
        let signing_key = SigningKey::random(&mut OsRng);
        let wrong_signing_key = SigningKey::random(&mut OsRng);
        let request = request_with_signing_key(&mut state, &signing_key);
        let wrong_signature = sign_pair_proof(&wrong_signing_key, &request.id, &request.nonce);

        let err = state
            .verify(&request.id, &wrong_signature)
            .expect_err("invalid signature must fail");
        assert!(matches!(err, Error::PairRequestInvalidProof));
    }

    #[test]
    fn verify_rejects_expired_request() {
        let mut state = PairingState::new();
        let signing_key = SigningKey::random(&mut OsRng);
        let request = request_with_signing_key(&mut state, &signing_key);
        let signature = sign_pair_proof(&signing_key, &request.id, &request.nonce);

        let pending = state.pending.get_mut(&request.id).expect("request exists");
        pending.expires_at = Instant::now() - Duration::from_secs(1);

        let err = state
            .verify(&request.id, &signature)
            .expect_err("expired verify must fail");
        assert!(matches!(err, Error::PairRequestExpired));
    }

    #[test]
    fn verify_rejects_non_pending_request() {
        let mut state = PairingState::new();
        let signing_key = SigningKey::random(&mut OsRng);
        let request = request_with_signing_key(&mut state, &signing_key);
        let signature = sign_pair_proof(&signing_key, &request.id, &request.nonce);

        state.verify(&request.id, &signature).expect("verify");
        state.approve(&request.id).expect("approve");

        let err = state
            .verify(&request.id, &signature)
            .expect_err("approved request cannot be verified again");
        assert!(matches!(
            err,
            Error::PairRequestNotPending(PairStatus::Approved)
        ));
    }

    #[test]
    fn approve_requires_verified_request() {
        let mut state = PairingState::new();
        let signing_key = SigningKey::random(&mut OsRng);
        let request = request_with_signing_key(&mut state, &signing_key);

        let err = state
            .approve(&request.id)
            .expect_err("unverified approve must fail");
        assert!(matches!(err, Error::PairRequestNotVerified));
    }

    #[test]
    fn verify_rejects_wrong_transcript_binding() {
        let mut state = PairingState::new();
        let signing_key = SigningKey::random(&mut OsRng);
        let request = request_with_signing_key(&mut state, &signing_key);

        // Sign a different transcript (wrong pair_id) with the correct key
        let wrong_transcript = format!("moltis-pairing-v1\nwrong-pair-id\n{}", request.nonce);
        let signature: Signature = signing_key.sign(wrong_transcript.as_bytes());
        let wrong_sig = URL_SAFE_NO_PAD.encode(signature.to_bytes());

        let err = state
            .verify(&request.id, &wrong_sig)
            .expect_err("wrong transcript must fail verification");
        assert!(matches!(err, Error::PairRequestInvalidProof));
    }
}
