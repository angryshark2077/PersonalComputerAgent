use base64::{engine::general_purpose::STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidProof;

/// Computes the cross-language Bridge proof.
///
/// The exact HMAC transcript is the 32 raw decoded nonce bytes, followed by the protocol version
/// as four unsigned big-endian bytes, followed by the exact UTF-8 bytes of `agent_version`.
/// A handshake responder signs the version it declares in its response envelope. The client must
/// authenticate that declaration before deciding whether the declared version is compatible.
///
/// # Panics
///
/// The HMAC implementation accepts keys of every length, so the fixed 32-byte key cannot trigger
/// the guarded construction panic.
#[must_use]
pub fn create_proof(
    secret: &[u8; 32],
    nonce: &[u8; 32],
    protocol_version: u32,
    agent_version: &str,
) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts a 32-byte secret");
    update_transcript(&mut mac, nonce, protocol_version, agent_version);
    STANDARD.encode(mac.finalize().into_bytes())
}

/// Verifies a base64 Bridge proof with the HMAC crate's constant-time verification API.
///
/// # Errors
///
/// Returns [`InvalidProof`] for malformed base64, a non-32-byte proof, or an HMAC mismatch.
pub fn verify_proof(
    secret: &[u8; 32],
    nonce: &[u8; 32],
    protocol_version: u32,
    agent_version: &str,
    proof: &str,
) -> Result<(), InvalidProof> {
    let proof = STANDARD.decode(proof).map_err(|_| InvalidProof)?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| InvalidProof)?;
    update_transcript(&mut mac, nonce, protocol_version, agent_version);
    mac.verify_slice(&proof).map_err(|_| InvalidProof)
}

fn update_transcript(
    mac: &mut HmacSha256,
    nonce: &[u8; 32],
    protocol_version: u32,
    agent_version: &str,
) {
    mac.update(nonce);
    mac.update(&protocol_version.to_be_bytes());
    mac.update(agent_version.as_bytes());
}
