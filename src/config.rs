use crate::{crypto::Crypto, error::AppError};
use secrecy::SecretString;
use std::{env, net::SocketAddr};

pub struct Config {
    pub database_url: String,
    pub bind: SocketAddr,
    pub crypto: Crypto,
    pub api_key: Option<SecretString>,
    pub dashboard_login: DashboardLogin,
    pub dashboard_secure_cookie: bool,
}

/// Where the dashboard password comes from. Login is required unless disabled.
pub enum DashboardLogin {
    /// An Argon2id hash in SQLite, chosen at first-run setup (the default).
    Stored,
    /// `ROUTER_DASHBOARD_PASSWORD`; it cannot be changed from the dashboard.
    Environment(SecretString),
    /// `ROUTER_DASHBOARD_AUTH=disabled`, for a dashboard behind an authenticating proxy.
    Disabled,
}

impl Config {
    pub fn database_url_from_env() -> String {
        env::var("ROUTER_DATABASE_URL").unwrap_or_else(|_| "sqlite://router.db".into())
    }

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
            .unwrap_or_else(|_| "127.0.0.1:14673".into())
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
        let dashboard_login = dashboard_login(
            env::var("ROUTER_DASHBOARD_AUTH").as_deref(),
            dashboard_password,
        )?;
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
            database_url: Self::database_url_from_env(),
            bind,
            crypto,
            api_key,
            dashboard_login,
            dashboard_secure_cookie,
        })
    }
}

/// Login is required unless explicitly disabled, and a fixed password cannot
/// be combined with turning login off.
fn dashboard_login(
    auth: Result<&str, &env::VarError>,
    password: Option<SecretString>,
) -> Result<DashboardLogin, AppError> {
    match (auth, password) {
        (Ok("required") | Err(env::VarError::NotPresent), Some(password)) => {
            Ok(DashboardLogin::Environment(password))
        }
        (Ok("required") | Err(env::VarError::NotPresent), None) => Ok(DashboardLogin::Stored),
        (Ok("disabled"), None) => Ok(DashboardLogin::Disabled),
        (Ok("disabled"), Some(_)) => Err(AppError::Configuration(
            "ROUTER_DASHBOARD_AUTH=disabled cannot be combined with ROUTER_DASHBOARD_PASSWORD"
                .into(),
        )),
        _ => Err(AppError::Configuration(
            "ROUTER_DASHBOARD_AUTH must be required or disabled".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_login_is_required_unless_explicitly_disabled() {
        let password = || Some(SecretString::from("fixed-dashboard-password"));
        let absent = Err(&env::VarError::NotPresent);
        assert!(matches!(
            dashboard_login(absent, None),
            Ok(DashboardLogin::Stored)
        ));
        assert!(matches!(
            dashboard_login(Ok("required"), None),
            Ok(DashboardLogin::Stored)
        ));
        assert!(matches!(
            dashboard_login(absent, password()),
            Ok(DashboardLogin::Environment(_))
        ));
        assert!(matches!(
            dashboard_login(Ok("disabled"), None),
            Ok(DashboardLogin::Disabled)
        ));
        for invalid in ["", "off", "Disabled", "false"] {
            assert!(dashboard_login(Ok(invalid), None).is_err(), "{invalid}");
        }
        assert!(dashboard_login(Ok("disabled"), password()).is_err());
        let not_unicode = env::VarError::NotUnicode("x".into());
        assert!(dashboard_login(Err(&not_unicode), None).is_err());
    }
}
