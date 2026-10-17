//! Safety and rollback framework for every miner write.
//!
//! - [`profile`]: `DeviceClass` / `DeviceProfile` identity.
//! - [`limits`]: per-class/model ranges (data: `limits.json`).
//! - [`policy`]: action classes, dry-run defaults, irreversible gate.
//! - [`snapshot`]: pre-write snapshots (memory + optional JSONL).
//! - [`paths`]: write paths + fallback chain (skip-with-reason, paths tried).
//! - [`health`]: post-change health watch.
//! - [`engine`]: the guarded sequence with automatic rollback.
//! - [`audit`]: append-only record of every attempt.
//!
//! Phase-1 start/stop/reboot go through [`policy::preflight`] and the audit
//! trail in the job runner; they change no settings, so they take no
//! snapshot and never roll back.

pub mod audit;
pub mod engine;
pub mod health;
pub mod limits;
pub mod paths;
pub mod policy;
pub mod profile;
pub mod settings;
pub mod snapshot;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod e2e_tests;

pub use audit::{AuditRecord, AuditSink, JsonlAudit, MemoryAudit, NullAudit};
pub use engine::{SafeWriteRequest, SafetyEngine, SafetyOptions, WriteOutcome, WriteStatus};
pub use policy::{preflight, ActionKind, Confirmation, Refusal};
pub use profile::{DeviceClass, DeviceProfile};
