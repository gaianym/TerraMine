//! Safety and rollback framework for every miner write.
//!
//! - [`profile`]: `DeviceClass` mapping from discovery identity.
//! - [`limits`]: per-class/model ranges (data: `limits.json`).
//! - [`policy`]: action classes, dry-run defaults, irreversible gate.
//! - [`snapshot`]: pre-write snapshots (memory + optional JSONL).
//! - [`asic`] / [`axeos`]: settings backends (asic-rs when it supports the
//!   change, the AxeOS HTTP override for Bitaxe).
//! - [`health`]: post-change health watch with typed failure causes.
//! - [`engine`]: the guarded per-miner sequence with automatic rollback.
//! - [`batch`]: canary halt, untouched-miner check, aggregated alert.
//! - [`audit`]: append-only record of every attempt.
//!
//! Phase-1 start/stop/reboot go through [`policy::preflight`], the device
//! cache and the audit trail in the job runner; they change no settings,
//! so they take no snapshot and never roll back.

pub mod asic;
pub mod audit;
pub mod axeos;
pub mod batch;
pub mod divert;
pub mod engine;
pub mod health;
pub mod limits;
#[cfg(any(test, feature = "mock-miner"))]
pub mod mock_native;
pub mod policy;
pub mod pools;
pub mod profile;
pub mod settings;
pub mod snapshot;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod axeos_e2e_tests;
#[cfg(test)]
mod fan_e2e_tests;
#[cfg(test)]
mod power_e2e_tests;
#[cfg(test)]
mod pools_e2e_tests;
#[cfg(test)]
mod divert_e2e_tests;
#[cfg(test)]
mod tuning_e2e_tests;

pub use audit::{AuditRecord, AuditSink, JsonlAudit, MemoryAudit, NullAudit};
pub use engine::{SafeWriteRequest, SafetyEngine, SafetyOptions, Target, WriteOutcome, WriteStatus};
pub use policy::{preflight, ActionKind, Confirmation, Refusal};
pub use profile::{DeviceClass, DeviceProfile};
