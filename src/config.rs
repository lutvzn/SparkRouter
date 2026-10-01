use crate::{crypto::Crypto, error::AppError};
use secrecy::SecretString;
use std::{env, net::SocketAddr};

pub struct Config {
    pub database_url: String,
    pub bind: SocketAddr,
    pub crypto: Crypto,
    pub api_key: Option<SecretString>,
    pub dashboard_password: Option<SecretString>,
    pub dashboard_secure_cookie: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, AppError> {
        let master = SecretString::from(env::var("ROUTER_MASTER_KEY").map_err(|_| {
            AppError::Configuration(
                "ROUTER_MASTER_KEY is required (base64-encoded 32 random bytes)".into(),
            )
        })?);
        let crypto = Crypto::from_base64(&master)?;
        let api_key = match env::var("ROUTER_API_KEY") {
            Ok(value) if value.trim().is_empty() => {
                return Err(AppError::Configuration(
                    "ROUTER_API_KEY must not be empty when set".into(),
                ));
            }
            Ok(value) => Some(SecretString::from(value)),
            Err(env::VarError::NotPresent) => None,
            Err(_) => {
                return Err(AppError::Configuration(
                    "ROUTER_API_KEY must be valid Unicode".into(),
                ));
            }
        };
        let bind = env::var("ROUTER_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".into())
            .parse()
            .map_err(|_| {
                AppError::Configuration("ROUTER_BIND must be an IP address and port".into())
            })?;
        let dashboard_password = match env::var("ROUTER_DASHBOARD_PASSWORD") {
            Ok(value) if value.len() < 16 || value.len() > 1024 || value.trim().is_empty() => {
                return Err(AppError::Configuration(
                    "ROUTER_DASHBOARD_PASSWORD must be 16–1024 bytes and not blank".into(),
                ));
            }
            Ok(value) => Some(SecretString::from(value)),
            Err(env::VarError::NotPresent) => None,
            Err(_) => {
                return Err(AppError::Configuration(
                    "ROUTER_DASHBOARD_PASSWORD must be valid Unicode".into(),
                ));
            }
        };
        let dashboard_secure_cookie = match env::var("ROUTER_DASHBOARD_SECURE_COOKIE") {
            Ok(value) if value == "true" => true,
            Ok(value) if value == "false" => false,
            Err(env::VarError::NotPresent) => false,
            _ => {
                return Err(AppError::Configuration(
                    "ROUTER_DASHBOARD_SECURE_COOKIE must be true or false".into(),
                ));
            }
        };
        Ok(Self {
            database_url: env::var("ROUTER_DATABASE_URL")
                .unwrap_or_else(|_| "sqlite://router.db".into()),
            bind,
            crypto,
            api_key,
            dashboard_password,
            dashboard_secure_cookie,
        })
    }
}
