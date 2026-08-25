use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn connection_code() -> String {
    let mut bytes = [0u8; 5];
    rand::rng().fill_bytes(&mut bytes);
    let bits = u64::from_be_bytes([0, 0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4]]);
    (0..8)
        .rev()
        .map(|shift| CROCKFORD[((bits >> (shift * 5)) & 31) as usize] as char)
        .collect()
}

pub fn normalize_code(value: &str) -> String {
    value
        .trim()
        .to_ascii_uppercase()
        .replace(['I', 'L'], "1")
        .replace('O', "0")
}

pub fn hash(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

pub fn verify_hash(value: &str, expected: &[u8]) -> bool {
    let actual = hash(value);
    actual.as_slice().ct_eq(expected).into()
}

pub fn pkce_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(hash(verifier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_crockford_and_case_insensitive() {
        for _ in 0..100 {
            let code = connection_code();
            assert_eq!(code.len(), 8);
            assert!(code.bytes().all(|b| CROCKFORD.contains(&b)));
            assert_eq!(normalize_code(&code.to_ascii_lowercase()), code);
        }
        assert_eq!(normalize_code("oil-"), "011-");
    }

    #[test]
    fn pkce_matches_rfc_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
