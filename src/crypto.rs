use crate::error::AppError;
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use secrecy::{ExposeSecret, SecretString};
use zeroize::Zeroizing;

// v1 envelope: version byte | random 96-bit nonce | ciphertext and GCM tag.
// Provider ID is authenticated too, preventing ciphertext swaps between records.
pub struct Crypto {
    key: Zeroizing<[u8; 32]>,
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
        Ok(Self { key })
    }

    pub fn encrypt_api_key(
        &self,
        provider_id: &str,
        secret: &SecretString,
    ) -> Result<Vec<u8>, AppError> {
        let cipher =
            Aes256Gcm::new_from_slice(self.key.as_ref()).map_err(|_| AppError::Encryption)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let encrypted = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: secret.expose_secret().as_bytes(),
                    aad: provider_id.as_bytes(),
                },
            )
            .map_err(|_| AppError::Encryption)?;
        let mut envelope = Vec::with_capacity(13 + encrypted.len());
        envelope.push(1);
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&encrypted);
        Ok(envelope)
    }

    pub fn decrypt_api_key(
        &self,
        provider_id: &str,
        envelope: &[u8],
    ) -> Result<SecretString, AppError> {
        if envelope.len() < 29 || envelope[0] != 1 {
            return Err(AppError::Encryption);
        }
        let cipher =
            Aes256Gcm::new_from_slice(self.key.as_ref()).map_err(|_| AppError::Encryption)?;
        let plain = Zeroizing::new(
            cipher
                .decrypt(
                    Nonce::from_slice(&envelope[1..13]),
                    Payload {
                        msg: &envelope[13..],
                        aad: provider_id.as_bytes(),
                    },
                )
                .map_err(|_| AppError::Encryption)?,
        );
        let text = std::str::from_utf8(&plain).map_err(|_| AppError::Encryption)?;
        Ok(SecretString::from(text))
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
