pub mod api;
pub mod app;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod health;
mod history;
mod key_limits;
pub(crate) mod provider_catalog;
pub mod providers;
pub mod routing;
pub mod web;

pub const APP_NAME: &str = "SparkRouter";
