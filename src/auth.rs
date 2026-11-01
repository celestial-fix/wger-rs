//! Password hashes (Django pbkdf2_sha256 and argon2) and RS256 JWTs.
//!
//! Access tokens follow allauth headless: `token_use=access`, `sub` is the user
//! pk as a string, plus `iat`, `exp`, `sid`, `jti`. Refresh tokens are minted
//! here (same shape, `token_use=refresh`). When the same RS256 key is shared
//! with Django, Rust also accepts Django-issued refresh tokens at cutover
//! (no phone re-login). If verification fails, phones must log in once. PowerSync tokens are a separate claim set (`aud=powersync`,
//! no `token_use`) and must not authenticate API requests.

use std::time::{SystemTime, UNIX_EPOCH};

use argon2::{Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version};
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use pbkdf2::pbkdf2_hmac;
use rsa::pkcs8::EncodePrivateKey;
use rsa::{BigUint, RsaPrivateKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

const ARGON2_MEMORY_KIB: u32 = 102_400;
const ARGON2_TIME: u32 = 2;
const ARGON2_PARALLELISM: u32 = 8;
const ARGON2_HASH_LEN: usize = 16;

#[derive(Clone)]
pub struct JwtKeys {
    pub encode: EncodingKey,
    pub decode: DecodingKey,
    pub public_jwk: Value,
    pub kid: String,
    pub alg: String,
}

pub fn load_keys(private_b64: &str, public_b64: &str) -> Result<JwtKeys, String> {
    let private_json = decode_jwk_b64(private_b64)?;
    let public_json = decode_jwk_b64(public_b64)?;
    let n = b64url_field(&private_json, "n")?;
    let e = b64url_field(&private_json, "e")?;
    let d = b64url_field(&private_json, "d")?;
    let p = b64url_field(&private_json, "p")?;
    let q = b64url_field(&private_json, "q")?;
    let mut key = RsaPrivateKey::from_components(
        BigUint::from_bytes_be(&n),
        BigUint::from_bytes_be(&e),
        BigUint::from_bytes_be(&d),
        vec![BigUint::from_bytes_be(&p), BigUint::from_bytes_be(&q)],
    )
    .map_err(|err| format!("JWT private key: {err}"))?;
    // CRT coefficients are required; prefer PEM — from_rsa_der rejects some DER shapes.
    key.precompute()
        .map_err(|err| format!("JWT precompute: {err}"))?;
    let pem = key
        .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
        .map_err(|err| format!("JWT PKCS8 PEM: {err}"))?;
    let encode_key = EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|err| format!("JWT EncodingKey: {err}"))?;
    let n_str = private_json
        .get("n")
        .and_then(|v| v.as_str())
        .ok_or("JWT private JWK missing n")?;
    let e_str = private_json
        .get("e")
        .and_then(|v| v.as_str())
        .ok_or("JWT private JWK missing e")?;
    let decode_key = DecodingKey::from_rsa_components(n_str, e_str)
        .map_err(|err| format!("JWT public components: {err}"))?;
    let kid = public_json
        .get("kid")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let alg = public_json
        .get("alg")
        .and_then(|v| v.as_str())
        .unwrap_or("RS256")
        .to_string();
    Ok(JwtKeys {
        encode: encode_key,
        decode: decode_key,
        public_jwk: public_json,
        kid,
        alg,
    })
}

fn decode_jwk_b64(raw: &str) -> Result<Value, String> {
    let bytes =
        decode_b64_flexible(raw.trim()).map_err(|err| format!("JWT key is not base64: {err}"))?;
    serde_json::from_slice(&bytes).map_err(|err| format!("JWT key is not JSON: {err}"))
}

fn b64url_field(jwk: &Value, name: &str) -> Result<Vec<u8>, String> {
    let text = jwk
        .get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("JWT JWK missing {name}"))?;
    decode_b64_flexible(text).map_err(|err| format!("JWT JWK {name}: {err}"))
}

/// Django's `urlsafe_b64encode` keeps `=` padding. JWK components are usually unpadded.
fn decode_b64_flexible(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let text = text.trim();
    URL_SAFE
        .decode(text.as_bytes())
        .or_else(|_| URL_SAFE_NO_PAD.decode(text.trim_end_matches('=').as_bytes()))
        .or_else(|_| STANDARD.decode(text.as_bytes()))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AccessClaims {
    pub iat: i64,
    pub exp: i64,
    pub sid: String,
    pub jti: String,
    pub token_use: String,
    pub sub: String,
}

pub fn mint_access(
    keys: &JwtKeys,
    user_id: i32,
    sid: &str,
    lifetime: i64,
) -> Result<String, String> {
    mint_use(keys, "access", user_id, sid, lifetime)
}

pub fn mint_refresh(
    keys: &JwtKeys,
    user_id: i32,
    sid: &str,
    lifetime: i64,
) -> Result<String, String> {
    mint_use(keys, "refresh", user_id, sid, lifetime)
}

fn mint_use(
    keys: &JwtKeys,
    token_use: &str,
    user_id: i32,
    sid: &str,
    lifetime: i64,
) -> Result<String, String> {
    let now = unix_now();
    let claims = AccessClaims {
        iat: now,
        exp: now + lifetime,
        sid: sid.to_string(),
        jti: Uuid::new_v4().to_string(),
        token_use: token_use.to_string(),
        sub: user_id.to_string(),
    };
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(keys.kid.clone());
    header.typ = Some("JWT".into());
    encode(&header, &claims, &keys.encode).map_err(|err| format!("sign jwt: {err}"))
}

pub fn mint_powersync(keys: &JwtKeys, user_id: i32, lifetime: i64) -> Result<String, String> {
    let now = unix_now();
    let claims = json!({
        "sub": user_id.to_string(),
        "iat": now,
        "aud": "powersync",
        "exp": now + lifetime,
    });
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(keys.kid.clone());
    header.typ = Some("JWT".into());
    encode(&header, &claims, &keys.encode).map_err(|err| format!("sign powersync: {err}"))
}

/// The token was absent, malformed, expired, or of the wrong `token_use`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidToken;

pub fn read_access(keys: &JwtKeys, token: &str) -> Result<AccessClaims, InvalidToken> {
    let mut validation = Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.validate_aud = false;
    validation.validate_exp = true;
    validation.leeway = 0;
    validation.set_required_spec_claims(&["exp"]);
    let data =
        decode::<AccessClaims>(token, &keys.decode, &validation).map_err(|_| InvalidToken)?;
    if data.claims.token_use != "access" {
        return Err(InvalidToken);
    }
    if data.claims.sub.is_empty() || data.claims.jti.is_empty() || data.claims.sid.is_empty() {
        return Err(InvalidToken);
    }
    Ok(data.claims)
}

pub fn read_refresh(keys: &JwtKeys, token: &str) -> Result<AccessClaims, InvalidToken> {
    let mut validation = Validation::new(jsonwebtoken::Algorithm::RS256);
    validation.validate_aud = false;
    validation.validate_exp = true;
    validation.leeway = 0;
    validation.set_required_spec_claims(&["exp"]);
    let data =
        decode::<AccessClaims>(token, &keys.decode, &validation).map_err(|_| InvalidToken)?;
    if data.claims.token_use != "refresh" {
        return Err(InvalidToken);
    }
    Ok(data.claims)
}

pub fn new_sid() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Verify a Django password hash. Returns whether it should be upgraded to argon2id.
pub fn verify_password(encoded: &str, password: &str) -> Result<bool, String> {
    if let Some(rest) = encoded.strip_prefix("pbkdf2_sha256$") {
        return verify_pbkdf2(rest, password);
    }
    // Django stores `argon2` + the PHC string, and the PHC string already
    // starts with `$argon2id$...`.
    if let Some(rest) = encoded.strip_prefix("argon2") {
        if rest.starts_with('$') {
            return verify_argon2(rest, password);
        }
    }
    if encoded.starts_with("$argon2") {
        return verify_argon2(encoded, password);
    }
    Ok(false)
}

/// True when the stored hash is not already Django's preferred argon2id encoding.
pub fn needs_rehash(encoded: &str) -> bool {
    !encoded.starts_with("argon2$argon2id$")
}

pub fn hash_argon2(password: &str) -> Result<String, String> {
    let params = Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_TIME,
        ARGON2_PARALLELISM,
        Some(ARGON2_HASH_LEN),
    )
    .map_err(|err| format!("argon2 params: {err}"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let hash = argon
        .hash_password(password.as_bytes())
        .map_err(|err| format!("argon2: {err}"))?;
    Ok(format!("argon2{hash}"))
}

fn verify_pbkdf2(rest: &str, password: &str) -> Result<bool, String> {
    let mut parts = rest.split('$');
    let iterations: u32 = parts
        .next()
        .ok_or("pbkdf2 missing iterations")?
        .parse()
        .map_err(|_| "pbkdf2 iterations".to_string())?;
    let salt = parts.next().ok_or("pbkdf2 missing salt")?;
    let digest_b64 = parts.next().ok_or("pbkdf2 missing digest")?;
    let expected = STANDARD
        .decode(digest_b64.as_bytes())
        .map_err(|err| format!("pbkdf2 digest: {err}"))?;
    let mut got = vec![0u8; expected.len().max(32)];
    pbkdf2_hmac::<Sha256>(
        password.as_bytes(),
        salt.as_bytes(),
        iterations,
        &mut got[..32],
    );
    if expected.len() != 32 {
        return Ok(false);
    }
    Ok(bool::from(got[..32].ct_eq(&expected)))
}

fn verify_argon2(phc: &str, password: &str) -> Result<bool, String> {
    let parsed = PasswordHash::new(phc).map_err(|err| format!("argon2 phc: {err}"))?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbkdf2_django_vector() {
        let encoded = "pbkdf2_sha256$1200000$abcxyz$Uqzl0m+myx+77kJdT0HsQQEd+KOhzyyCyLwUwAJJVDQ=";
        assert!(verify_password(encoded, "s3cret-pass").unwrap());
        assert!(!verify_password(encoded, "nope").unwrap());
        assert!(needs_rehash(encoded));
    }

    #[test]
    fn argon2_django_vector() {
        // argon2-cffi hash_len=16, salt bytes `saltsaltsaltsalt`, Django's default costs.
        let encoded =
            "argon2$argon2id$v=19$m=102400,t=2,p=8$c2FsdHNhbHRzYWx0c2FsdA$mMsyy5xJHrHHIf+LVyDjtw";
        assert!(verify_password(encoded, "s3cret-pass").unwrap());
        assert!(!verify_password(encoded, "nope").unwrap());
        assert!(!needs_rehash(encoded));
    }

    #[test]
    fn rehash_roundtrip() {
        let encoded = hash_argon2("s3cret-pass").unwrap();
        assert!(encoded.starts_with("argon2$argon2id$"));
        assert!(verify_password(&encoded, "s3cret-pass").unwrap());
        assert!(!verify_password(&encoded, "other").unwrap());
    }
}
