//! Per-firmware driver overrides where asic-rs capability flags or
//! HTTP methods don't match live hardware.

pub mod auth;
pub mod bitaxe;
pub mod s9;

pub use auth::DigestCredentials;
pub use bitaxe::{axeos_caps, parse_axeos_version, BitaxeDriver, PAUSE_RESUME_MIN};
pub use s9::S9StockDriver;
