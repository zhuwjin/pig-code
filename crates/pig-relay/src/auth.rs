use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac as _};
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

type HmacSha256 = Hmac<Sha256>;

const PBKDF2_ITERATIONS: u32 = 100_000;
const SALT_LEN: usize = 16;
const DERIVED_LEN: usize = 32;

pub fn hash_password(password: &str) -> String {
    let mut salt = [0_u8; SALT_LEN];
    rand::rng().fill_bytes(&mut salt);
    let derived = pbkdf2_hmac_sha256(password.as_bytes(), &salt, PBKDF2_ITERATIONS);
    format!(
        "pbkdf2-sha256${PBKDF2_ITERATIONS}${}${}",
        STANDARD.encode(salt),
        STANDARD.encode(derived)
    )
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    let Some("pbkdf2-sha256") = parts.next() else {
        return false;
    };
    let Some(iterations) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    if iterations < 10_000 {
        return false;
    }
    let Some(salt) = parts.next().and_then(|value| STANDARD.decode(value).ok()) else {
        return false;
    };
    let Some(expected) = parts.next().and_then(|value| STANDARD.decode(value).ok()) else {
        return false;
    };
    if parts.next().is_some() || expected.len() != DERIVED_LEN {
        return false;
    }
    let actual = pbkdf2_hmac_sha256(password.as_bytes(), &salt, iterations);
    bool::from(actual.ct_eq(expected.as_slice()))
}

pub fn generate_secret_token(prefix: &str) -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("{prefix}_{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn generate_user_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut raw = [0_u8; 8];
    rand::rng().fill_bytes(&mut raw);
    let chars: Vec<char> = raw
        .iter()
        .map(|byte| ALPHABET[(*byte as usize) % ALPHABET.len()] as char)
        .collect();
    format!(
        "{}-{}",
        chars[..4].iter().collect::<String>(),
        chars[4..].iter().collect::<String>()
    )
}

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub fn compute_websocket_accept(key: &str) -> String {
    tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.trim().as_bytes())
}

fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; DERIVED_LEN] {
    let mut mac = HmacSha256::new_from_slice(password).expect("HMAC accepts any key length");
    mac.update(salt);
    mac.update(&1_u32.to_be_bytes());
    let mut u = mac.finalize().into_bytes();
    let mut result = [0_u8; DERIVED_LEN];
    result.copy_from_slice(&u);
    for _ in 1..iterations {
        let mut next = HmacSha256::new_from_slice(password).expect("HMAC accepts any key length");
        next.update(&u);
        u = next.finalize().into_bytes();
        for (dst, src) in result.iter_mut().zip(u.iter()) {
            *dst ^= *src;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_and_verify_round_trip() {
        let hash = hash_password("correct-horse-battery-staple");
        assert!(verify_password("correct-horse-battery-staple", &hash));
        assert!(!verify_password("wrong-password", &hash));
    }

    #[test]
    fn websocket_accept_matches_rfc6455_vector() {
        assert_eq!(
            compute_websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }
}
