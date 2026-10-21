use crate::error::AppError;
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use secrecy::{ExposeSecret, SecretString};
use sha2::Sha256;
use zeroize::Zeroizing;

// v1 envelope: version byte | random 96-bit nonce | ciphertext and GCM tag.
// Provider ID is authenticated too, preventing ciphertext swaps between records.
pub struct Crypto {
    key: Zeroizing<[u8; 32]>,
    /// HMAC key for fingerprints. It is the master key until the first
    /// rotation, which seals it in SQLite so stored gateway-key fingerprints,
    /// which cannot be recomputed without the keys, stay valid.
    fingerprint_key: Zeroizing<[u8; 32]>,
}

impl Crypto {
    pub fn from_base64(encoded: &SecretString) -> Result<Self, AppError> {
        let decoded = Zeroizing::new(STANDARD.decode(encoded.expose_secret()).map_err(|_| {
            AppError::Configuration(
                "ROUTER_MASTER_KEY must be base64-encoded 32 random bytes".into(),
            )
        })?);
        let mut key = Zeroizing::new([0u8; 32]);
        if decoded.len() != key.len() {
            return Err(AppError::Configuration(
                "ROUTER_MASTER_KEY must decode to exactly 32 bytes".into(),
            ));
        }
        key.copy_from_slice(&decoded);
        Ok(Self {
            fingerprint_key: key.clone(),
            key,
        })
    }

    pub fn encrypt_api_key(
        &self,
        provider_id: &str,
        secret: &SecretString,
    ) -> Result<Vec<u8>, AppError> {
        self.seal(provider_id.as_bytes(), secret.expose_secret().as_bytes())
    }

    pub fn decrypt_api_key(
        &self,
        provider_id: &str,
        envelope: &[u8],
    ) -> Result<SecretString, AppError> {
        let plain = self.open(provider_id.as_bytes(), envelope)?;
        let text = std::str::from_utf8(&plain).map_err(|_| AppError::Encryption)?;
        Ok(SecretString::from(text))
    }

    /// Encrypt `plain` into a v1 envelope bound to `context`.
    pub(crate) fn seal(&self, context: &[u8], plain: &[u8]) -> Result<Vec<u8>, AppError> {
        let cipher =
            Aes256Gcm::new_from_slice(self.key.as_ref()).map_err(|_| AppError::Encryption)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let encrypted = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plain,
                    aad: context,
                },
            )
            .map_err(|_| AppError::Encryption)?;
        let mut envelope = Vec::with_capacity(13 + encrypted.len());
        envelope.push(1);
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&encrypted);
        Ok(envelope)
    }

    /// Decrypt a v1 envelope sealed with this key for `context`.
    pub(crate) fn open(
        &self,
        context: &[u8],
        envelope: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>, AppError> {
        if envelope.len() < 29 || envelope[0] != 1 {
            return Err(AppError::Encryption);
        }
        let cipher =
            Aes256Gcm::new_from_slice(self.key.as_ref()).map_err(|_| AppError::Encryption)?;
        Ok(Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(&envelope[1..13]),
                    Payload {
                        msg: &envelope[13..],
                        aad: context,
                    },
                )
                .map_err(|_| AppError::Encryption)?,
        ))
    }

    pub(crate) fn master_key(&self) -> &[u8; 32] {
        &self.key
    }

    pub(crate) fn set_fingerprint_key(&mut self, key: &[u8]) -> Result<(), AppError> {
        if key.len() != self.fingerprint_key.len() {
            return Err(AppError::Encryption);
        }
        self.fingerprint_key.copy_from_slice(key);
        Ok(())
    }

    /// True when both values hold the same master key.
    pub(crate) fn same_master_key(&self, other: &Self) -> bool {
        use subtle::ConstantTimeEq as _;
        bool::from(self.key.as_slice().ct_eq(other.key.as_slice()))
    }

    /// HMAC-SHA256 fingerprint of a gateway API key. The master key is the HMAC
    /// key, so a database copy cannot be matched without it. Not reversible.
    pub fn gateway_key_fingerprint(&self, secret: &str) -> Result<String, AppError> {
        self.fingerprint(b"sparkrouter.gateway-api-key.v1\0", secret)
    }

    pub(crate) fn dashboard_password_fingerprint(&self, secret: &str) -> Result<String, AppError> {
        self.fingerprint(b"sparkrouter.dashboard-password.v1\0", secret)
    }

    pub(crate) fn affinity_fingerprint(&self, value: &[u8]) -> Result<String, AppError> {
        self.fingerprint_bytes(b"sparkrouter.routing-affinity.v1\0", value)
    }

    fn fingerprint(&self, domain: &[u8], secret: &str) -> Result<String, AppError> {
        self.fingerprint_bytes(domain, secret.as_bytes())
    }

    fn fingerprint_bytes(&self, domain: &[u8], value: &[u8]) -> Result<String, AppError> {
        let mut mac =
            <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(self.fingerprint_key.as_ref())
                .map_err(|_| AppError::Encryption)?;
        <Hmac<Sha256> as Mac>::update(&mut mac, domain);
        <Hmac<Sha256> as Mac>::update(&mut mac, value);
        Ok(STANDARD.encode(<Hmac<Sha256> as Mac>::finalize(mac).into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encryption_round_trip_random_nonces_and_authenticated_context() {
        let crypto = Crypto::from_base64(&SecretString::from(STANDARD.encode([42; 32]))).unwrap();
        let key = SecretString::from("sk-private-value");
        let a = crypto.encrypt_api_key("p1", &key).unwrap();
        let b = crypto.encrypt_api_key("p1", &key).unwrap();
        assert_ne!(a, b);
        assert!(!a.windows(16).any(|w| w == b"sk-private-value"));
        assert_eq!(
            crypto.decrypt_api_key("p1", &a).unwrap().expose_secret(),
            key.expose_secret()
        );
        assert!(crypto.decrypt_api_key("p2", &a).is_err());
        let other = Crypto::from_base64(&SecretString::from(STANDARD.encode([43; 32]))).unwrap();
        assert!(other.decrypt_api_key("p1", &a).is_err());
        let mut tampered = a;
        tampered[15] ^= 1;
        assert!(crypto.decrypt_api_key("p1", &tampered).is_err());
        assert!(crypto.decrypt_api_key("p1", &[1, 2]).is_err());
    }

    #[test]
    fn gateway_key_fingerprint_is_stable_and_not_the_secret() {
        let crypto = Crypto::from_base64(&SecretString::from(STANDARD.encode([42; 32]))).unwrap();
        let first = crypto.gateway_key_fingerprint("sk-example").unwrap();
        let second = crypto.gateway_key_fingerprint("sk-example").unwrap();
        assert_ne!(
            first,
            crypto.dashboard_password_fingerprint("sk-example").unwrap()
        );
        assert_eq!(first, second);
        assert!(!first.contains("sk-example"));
        let other = Crypto::from_base64(&SecretString::from(STANDARD.encode([43; 32]))).unwrap();
        assert_ne!(other.gateway_key_fingerprint("sk-example").unwrap(), first);
        assert_ne!(crypto.gateway_key_fingerprint("sk-other").unwrap(), first);
    }

    #[test]
    fn master_key_requires_exactly_256_bits() {
        for invalid in [
            "not base64".to_owned(),
            STANDARD.encode([0; 16]),
            STANDARD.encode([0; 64]),
        ] {
            assert!(Crypto::from_base64(&SecretString::from(invalid)).is_err());
        }
    }
}
