//! Bearer token authentication for the SSE transport (RFC-007 §4).
//!
//! # Token format
//! ```text
//! base64url_no_pad(tenant_id_bytes) ":" scope ":" unix_ts_decimal "." base64url_no_pad(hmac_sha256_bytes)
//! ```
//!
//! # Scope
//! - `mcp`     — may call MCP tools via /sse and /rpc
//! - `metrics` — may read /metrics
//!
//! # Key rotation
//! Multiple signing keys are tried in sequence so that a rotation grace period is
//! supported. The first key that produces a valid MAC wins.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{KeyInit, Mac, SimpleHmac};
use sha2::Sha256;

type HmacSha256 = SimpleHmac<Sha256>;

/// A successfully verified bearer token.
#[derive(Debug, Clone)]
pub struct VerifiedToken {
    pub tenant_id: String,
    pub scope: TokenScope,
}

/// Scopes carried by a bearer token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenScope {
    Mcp,
    Metrics,
}

/// Errors that can occur during token verification.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing Authorization header")]
    MissingHeader,

    #[error("malformed Authorization header (expected 'Bearer <token>')")]
    BadHeaderFormat,

    #[error("malformed token: {0}")]
    BadTokenFormat(String),

    #[error("invalid tenant id: {0}")]
    InvalidTenantId(String),

    #[error("token timestamp out of allowed window")]
    Expired,

    #[error("HMAC verification failed")]
    BadMac,

    #[error("unknown scope '{0}'")]
    UnknownScope(String),

    #[error("insufficient scope: required {required}, got {got}")]
    InsufficientScope { required: String, got: String },

    #[error("no signing keys available")]
    NoSigningKeys,
}

/// Timestamp window around the current time that is accepted as valid.
const TIMESTAMP_WINDOW: Duration = Duration::from_secs(60);

/// Regex-free tenant_id validation: `^[a-z0-9-]{1,64}$`
fn is_valid_tenant_id(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Parse and verify a raw bearer token string against the provided signing keys.
///
/// Returns [`VerifiedToken`] on success or an [`AuthError`] describing the failure.
pub fn verify_token(token: &str, signing_keys: &[[u8; 32]]) -> Result<VerifiedToken, AuthError> {
    if signing_keys.is_empty() {
        return Err(AuthError::NoSigningKeys);
    }

    // Split on the last '.' to separate payload from MAC.
    let dot_pos = token.rfind('.').ok_or_else(|| {
        AuthError::BadTokenFormat("missing '.' separator between payload and MAC".into())
    })?;
    let payload = &token[..dot_pos];
    let mac_b64 = &token[dot_pos + 1..];

    // Decode MAC.
    let mac_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(mac_b64)
        .map_err(|e| AuthError::BadTokenFormat(format!("MAC base64 decode error: {e}")))?;

    // Split payload on ':' into exactly 3 parts.
    let parts: Vec<&str> = payload.splitn(3, ':').collect();
    if parts.len() != 3 {
        return Err(AuthError::BadTokenFormat(
            "payload must have exactly 3 ':'-separated fields".into(),
        ));
    }
    let tenant_b64 = parts[0];
    let scope_str = parts[1];
    let unix_ts_str = parts[2];

    // Decode tenant_id.
    let tenant_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(tenant_b64)
        .map_err(|e| AuthError::BadTokenFormat(format!("tenant_id base64 decode error: {e}")))?;
    let tenant_id = String::from_utf8(tenant_bytes)
        .map_err(|e| AuthError::BadTokenFormat(format!("tenant_id UTF-8 decode error: {e}")))?;

    // Validate tenant_id charset.
    if !is_valid_tenant_id(&tenant_id) {
        return Err(AuthError::InvalidTenantId(tenant_id));
    }

    // Parse scope.
    let scope = match scope_str {
        "mcp" => TokenScope::Mcp,
        "metrics" => TokenScope::Metrics,
        other => return Err(AuthError::UnknownScope(other.to_string())),
    };

    // Parse and validate timestamp.
    let unix_ts: u64 = unix_ts_str
        .parse()
        .map_err(|e| AuthError::BadTokenFormat(format!("timestamp parse error: {e}")))?;
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    let delta = now_secs.abs_diff(unix_ts);
    if delta > TIMESTAMP_WINDOW.as_secs() {
        return Err(AuthError::Expired);
    }

    // Try each signing key (key rotation grace period).
    let mac_valid = signing_keys.iter().any(|key| {
        let mut mac =
            HmacSha256::new_from_slice(key.as_slice()).expect("HMAC-SHA256 accepts any key length");
        mac.update(payload.as_bytes());
        mac.verify_slice(&mac_bytes).is_ok()
    });

    if !mac_valid {
        return Err(AuthError::BadMac);
    }

    Ok(VerifiedToken { tenant_id, scope })
}

/// Fetch signing keys from the environment or OCI IMDS.
///
/// Local dev: set `TRAVSR_SIGNING_KEY_HEX` to a 64-char hex string (32 bytes).
/// Cloud: set `TRAVSR_VAULT_SECRET_OCID` — IMDS fetch not yet implemented.
pub fn fetch_signing_keys() -> anyhow::Result<Vec<[u8; 32]>> {
    if let Ok(hex) = std::env::var("TRAVSR_SIGNING_KEY_HEX") {
        let hex = hex.trim();
        if hex.len() != 64 {
            anyhow::bail!(
                "TRAVSR_SIGNING_KEY_HEX must be exactly 64 hex chars (32 bytes), got {}",
                hex.len()
            );
        }
        let mut key = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let byte = u8::from_str_radix(
                std::str::from_utf8(chunk).map_err(|e| anyhow::anyhow!("invalid hex: {e}"))?,
                16,
            )
            .map_err(|e| anyhow::anyhow!("invalid hex char at byte {i}: {e}"))?;
            key[i] = byte;
        }
        return Ok(vec![key]);
    }

    if std::env::var("TRAVSR_VAULT_SECRET_OCID").is_ok() {
        // TODO: implement OCI Vault IMDS fetch via reqwest when deploying to OCI.
        // For now, return a clear error so local dev always uses TRAVSR_SIGNING_KEY_HEX.
        anyhow::bail!(
            "IMDS fetch from OCI Vault is not yet implemented — \
             set TRAVSR_SIGNING_KEY_HEX for local dev"
        );
    }

    anyhow::bail!(
        "no signing key configured — set TRAVSR_SIGNING_KEY_HEX (64-char hex) for local dev"
    )
}

/// Map an [`AuthError`] to an HTTP status code.
pub fn auth_error_status(e: &AuthError) -> axum::http::StatusCode {
    use axum::http::StatusCode;
    match e {
        AuthError::MissingHeader | AuthError::BadMac | AuthError::Expired => {
            StatusCode::UNAUTHORIZED
        }
        AuthError::BadHeaderFormat | AuthError::BadTokenFormat(_) => StatusCode::BAD_REQUEST,
        AuthError::InvalidTenantId(_)
        | AuthError::InsufficientScope { .. }
        | AuthError::UnknownScope(_) => StatusCode::FORBIDDEN,
        AuthError::NoSigningKeys => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key() -> [u8; 32] {
        [0x42u8; 32]
    }

    fn make_token(tenant: &str, scope: &str, ts: u64, key: &[u8; 32]) -> String {
        let tenant_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tenant);
        let payload = format!("{}:{}:{}", tenant_b64, scope, ts);
        let mut mac = HmacSha256::new_from_slice(key.as_slice()).unwrap();
        mac.update(payload.as_bytes());
        let mac_bytes = mac.finalize().into_bytes();
        let mac_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac_bytes);
        format!("{}.{}", payload, mac_b64)
    }

    fn now_ts() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn valid_mcp_token_verifies() {
        let key = make_key();
        let token = make_token("acme-corp", "mcp", now_ts(), &key);
        let result = verify_token(&token, &[key]);
        assert!(result.is_ok(), "valid token should verify: {:?}", result);
        let vt = result.unwrap();
        assert_eq!(vt.tenant_id, "acme-corp");
        assert_eq!(vt.scope, TokenScope::Mcp);
    }

    #[test]
    fn valid_metrics_token_verifies() {
        let key = make_key();
        let token = make_token("acme-corp", "metrics", now_ts(), &key);
        let vt = verify_token(&token, &[key]).unwrap();
        assert_eq!(vt.scope, TokenScope::Metrics);
    }

    #[test]
    fn bad_mac_returns_error() {
        let key = make_key();
        let mut token = make_token("acme-corp", "mcp", now_ts(), &key);
        // Corrupt the second-to-last character of the MAC. The final base64url
        // character of a 32-byte MAC has constrained padding bits (only 16 valid
        // values), so replacing it with an arbitrary char can produce a
        // non-canonical encoding that the decoder rejects as BadTokenFormat
        // instead of BadMac. The second-to-last char has no such constraint.
        let len = token.len();
        let replacement = if &token[len - 2..len - 1] == "A" {
            "B"
        } else {
            "A"
        };
        token = format!("{}{}{}", &token[..len - 2], replacement, &token[len - 1..]);
        let result = verify_token(&token, &[key]);
        assert!(matches!(result, Err(AuthError::BadMac)));
    }

    #[test]
    fn expired_token_rejected() {
        let key = make_key();
        let old_ts = now_ts().saturating_sub(120); // 2 minutes ago
        let token = make_token("acme-corp", "mcp", old_ts, &key);
        let result = verify_token(&token, &[key]);
        assert!(matches!(result, Err(AuthError::Expired)));
    }

    #[test]
    fn invalid_tenant_id_rejected() {
        let key = make_key();
        // Contains uppercase — not allowed.
        let token = make_token("ACME", "mcp", now_ts(), &key);
        let result = verify_token(&token, &[key]);
        assert!(matches!(result, Err(AuthError::InvalidTenantId(_))));
    }

    #[test]
    fn unknown_scope_rejected() {
        let key = make_key();
        let token = make_token("acme-corp", "admin", now_ts(), &key);
        let result = verify_token(&token, &[key]);
        assert!(matches!(result, Err(AuthError::UnknownScope(_))));
    }

    #[test]
    fn key_rotation_tries_all_keys() {
        let old_key = [0x11u8; 32];
        let new_key = [0x22u8; 32];
        // Token signed with old_key.
        let token = make_token("acme-corp", "mcp", now_ts(), &old_key);
        // Both keys provided — should succeed via old_key.
        let result = verify_token(&token, &[new_key, old_key]);
        assert!(
            result.is_ok(),
            "rotation: old key should still verify: {:?}",
            result
        );
    }

    #[test]
    fn no_keys_returns_error() {
        let key = make_key();
        let token = make_token("acme-corp", "mcp", now_ts(), &key);
        let result = verify_token(&token, &[]);
        assert!(matches!(result, Err(AuthError::NoSigningKeys)));
    }

    #[test]
    fn is_valid_tenant_id_accepts_valid() {
        assert!(is_valid_tenant_id("acme-corp"));
        assert!(is_valid_tenant_id("a"));
        assert!(is_valid_tenant_id("abc-123"));
        assert!(is_valid_tenant_id(&"a".repeat(64)));
    }

    #[test]
    fn is_valid_tenant_id_rejects_invalid() {
        assert!(!is_valid_tenant_id(""));
        assert!(!is_valid_tenant_id("UPPER"));
        assert!(!is_valid_tenant_id("has space"));
        assert!(!is_valid_tenant_id(&"a".repeat(65)));
    }
}
