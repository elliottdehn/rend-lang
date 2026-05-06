//! EOA-style auth: secp256k1 ECDSA over keccak256 of a canonical
//! request, with per-address monotonic nonces for replay protection.
//!
//! ## Wire format
//!
//! Two headers on every authed request:
//!
//! ```text
//! X-Rend-Sig: 0x<130 hex chars>     // 65 bytes: r || s || v(0|1)
//! X-Rend-Nonce: <decimal u64>        // strictly > last accepted for this address
//! ```
//!
//! The signed message is keccak256 of:
//!
//! ```text
//! "REND/v1\n" || method || "\n" || path || "\n" || nonce_be_u64 || "\n" || body
//! ```
//!
//! On success the server recovers a 20-byte address (Ethereum-style:
//! keccak256(uncompressed_pubkey[1..]).last_20) and requires that
//! `:org == "0x" + hex(address)`. Address-as-org means there is no
//! registration step — your private key is your tenant identity.
//!
//! Failed/reverted txs still consume the nonce (the bump happens in
//! the auth middleware before the handler runs), matching EOA
//! semantics on chains like Ethereum.

use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use rend_rocksdb::NonceResult;
use secp256k1::{
    ecdsa::{RecoverableSignature, RecoveryId},
    Message, PublicKey, Secp256k1, SecretKey,
};
use tiny_keccak::{Hasher, Keccak};

use crate::ServerState;

pub const SIG_HEADER: &str = "x-rend-sig";
pub const NONCE_HEADER: &str = "x-rend-nonce";
const DOMAIN: &[u8] = b"REND/v1\n";
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Ethereum-style address: keccak256 of the 64-byte uncompressed
/// pubkey (X || Y, no 0x04 prefix), last 20 bytes.
pub fn address_from_pubkey(pubkey: &PublicKey) -> [u8; 20] {
    let serialized = pubkey.serialize_uncompressed();
    let mut hasher = Keccak::v256();
    hasher.update(&serialized[1..]);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&out[12..]);
    addr
}

fn message_hash(method: &str, path: &str, nonce: u64, body: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak::v256();
    hasher.update(DOMAIN);
    hasher.update(method.as_bytes());
    hasher.update(b"\n");
    hasher.update(path.as_bytes());
    hasher.update(b"\n");
    hasher.update(&nonce.to_be_bytes());
    hasher.update(b"\n");
    hasher.update(body);
    let mut out = [0u8; 32];
    hasher.finalize(&mut out);
    out
}

/// Client-side wallet. Lives in the server crate so smoke tests and
/// the bench harness can sign requests against the same canonical
/// message format the middleware checks.
pub struct Eoa {
    secret: SecretKey,
    pub address: [u8; 20],
    secp: Secp256k1<secp256k1::All>,
}

impl Eoa {
    pub fn new() -> Self {
        let secp = Secp256k1::new();
        let mut rng = rand::thread_rng();
        let (secret, public) = secp.generate_keypair(&mut rng);
        let address = address_from_pubkey(&public);
        Self { secret, address, secp }
    }

    pub fn from_secret(bytes: [u8; 32]) -> Self {
        let secp = Secp256k1::new();
        let secret = SecretKey::from_slice(&bytes).expect("32-byte secret");
        let public = secret.public_key(&secp);
        let address = address_from_pubkey(&public);
        Self { secret, address, secp }
    }

    pub fn address_hex(&self) -> String {
        format!("0x{}", hex::encode(self.address))
    }

    /// Sign a canonical request. Returns 65 bytes (r || s || v) where
    /// v is the secp256k1 recovery id (0 or 1).
    pub fn sign(&self, method: &str, path: &str, nonce: u64, body: &[u8]) -> [u8; 65] {
        let hash = message_hash(method, path, nonce, body);
        let msg = Message::from_digest(hash);
        let sig = self.secp.sign_ecdsa_recoverable(&msg, &self.secret);
        let (recid, compact) = sig.serialize_compact();
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&compact);
        out[64] = recid.to_i32() as u8;
        out
    }
}

impl Default for Eoa {
    fn default() -> Self { Self::new() }
}

pub fn recover_address(
    method: &str,
    path: &str,
    nonce: u64,
    body: &[u8],
    sig: &[u8; 65],
) -> Option<[u8; 20]> {
    let hash = message_hash(method, path, nonce, body);
    let msg = Message::from_digest(hash);
    let recid = RecoveryId::from_i32(sig[64] as i32).ok()?;
    let signature = RecoverableSignature::from_compact(&sig[..64], recid).ok()?;
    let secp = Secp256k1::verification_only();
    let pubkey = secp.recover_ecdsa(&msg, &signature).ok()?;
    Some(address_from_pubkey(&pubkey))
}

pub async fn require_auth(
    State(state): State<ServerState>,
    req: Request,
    next: Next,
) -> Response {
    match check_auth(&state, req).await {
        Ok(req) => next.run(req).await,
        Err(resp) => resp,
    }
}

async fn check_auth(state: &ServerState, req: Request) -> Result<Request, Response> {
    let method = req.method().as_str().to_string();
    let uri_path = req.uri().path().to_string();

    let sig_hdr = req.headers().get(SIG_HEADER)
        .ok_or_else(|| unauthorized("missing X-Rend-Sig"))?;
    let sig_bytes = parse_sig(sig_hdr).map_err(|m| unauthorized(&m))?;

    let nonce: u64 = req.headers().get(NONCE_HEADER)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| unauthorized("missing or malformed X-Rend-Nonce"))?;

    let (parts, body) = req.into_parts();
    let body_bytes = axum::body::to_bytes(body, MAX_BODY).await
        .map_err(|e| unauthorized(&format!("body read failed: {e}")))?;

    let recovered = recover_address(&method, &uri_path, nonce, &body_bytes, &sig_bytes)
        .ok_or_else(|| unauthorized("signature recovery failed"))?;

    let org = path_org(&uri_path)
        .ok_or_else(|| unauthorized("no org segment in path"))?;
    let expected = format!("0x{}", hex::encode(recovered));
    if !org.eq_ignore_ascii_case(&expected) {
        return Err(unauthorized(&format!(
            "signature recovers to {expected}, not org {org}"
        )));
    }

    match state.kv().bump_nonce(&recovered, nonce) {
        Ok(NonceResult::Bumped) => {}
        Ok(NonceResult::TooLow { stored }) => {
            return Err(unauthorized(&format!(
                "nonce too low: submitted {nonce}, stored {stored}"
            )));
        }
        Err(e) => {
            tracing::error!(error = %e, "bump_nonce failed");
            return Err(internal("nonce store error"));
        }
    }

    Ok(Request::from_parts(parts, Body::from(body_bytes)))
}

fn parse_sig(h: &HeaderValue) -> Result<[u8; 65], String> {
    let s = h.to_str().map_err(|_| "X-Rend-Sig must be ASCII".to_string())?;
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).map_err(|_| "X-Rend-Sig is not valid hex".to_string())?;
    if bytes.len() != 65 {
        return Err(format!("X-Rend-Sig must be 65 bytes, got {}", bytes.len()));
    }
    let mut out = [0u8; 65];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn path_org(path: &str) -> Option<&str> {
    let mut parts = path.trim_start_matches('/').split('/');
    if parts.next() != Some("v1") { return None; }
    if parts.next() != Some("orgs") { return None; }
    parts.next()
}

fn unauthorized(msg: &str) -> Response {
    (StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({"error": msg})))
        .into_response()
}

fn internal(msg: &str) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": msg})))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_sign_recover() {
        let wallet = Eoa::new();
        let body = br#"{"hello":"world"}"#;
        let sig = wallet.sign("POST", "/v1/orgs/0xabc/tx", 7, body);
        let recovered = recover_address(
            "POST", "/v1/orgs/0xabc/tx", 7, body, &sig,
        ).unwrap();
        assert_eq!(recovered, wallet.address);
    }

    #[test]
    fn tampered_body_breaks_recovery() {
        let wallet = Eoa::new();
        let sig = wallet.sign("POST", "/p", 1, b"original");
        let recovered = recover_address("POST", "/p", 1, b"tampered", &sig);
        // Recovery itself can succeed (the math doesn't care) but the
        // recovered address won't match the wallet.
        assert!(recovered.map_or(true, |a| a != wallet.address));
    }

    #[test]
    fn changed_nonce_breaks_recovery() {
        let wallet = Eoa::new();
        let sig = wallet.sign("POST", "/p", 1, b"x");
        let recovered = recover_address("POST", "/p", 2, b"x", &sig);
        assert!(recovered.map_or(true, |a| a != wallet.address));
    }

    #[test]
    fn from_secret_is_deterministic() {
        let secret = [42u8; 32];
        let a = Eoa::from_secret(secret);
        let b = Eoa::from_secret(secret);
        assert_eq!(a.address, b.address);
    }

    #[test]
    fn path_org_parses_v1_segments() {
        assert_eq!(path_org("/v1/orgs/0xabc/tx"), Some("0xabc"));
        assert_eq!(path_org("/v1/orgs/0xdeadbeef/artifacts/abcd"), Some("0xdeadbeef"));
        assert_eq!(path_org("/v1/orgs"), None);
        assert_eq!(path_org("/healthz"), None);
        assert_eq!(path_org("/v2/orgs/0x/tx"), None);
    }
}
