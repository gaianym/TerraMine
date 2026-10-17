//! Action classes and the pre-flight gate every write passes through.
//!
//! - Start / stop / reboot keep their existing behaviour (dry run is
//!   chosen by the caller; the UI defaults it on).
//! - Every new write class defaults to dry run.
//! - Irreversible actions (factory reset, firmware, password) are refused
//!   unless the operator typed the action name exactly AND set
//!   `allow_irreversible`, and only for one device at a time.

use serde::{Deserialize, Serialize};

use crate::control::driver::MinerAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Start,
    Stop,
    Reboot,
    ApplySettings,
    FactoryReset,
    FirmwareUpdate,
    ChangePassword,
}

impl ActionKind {
    pub const ALL: [ActionKind; 7] = [
        Self::Start,
        Self::Stop,
        Self::Reboot,
        Self::ApplySettings,
        Self::FactoryReset,
        Self::FirmwareUpdate,
        Self::ChangePassword,
    ];

    /// Also the exact text an operator must type to confirm.
    pub fn name(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Reboot => "reboot",
            Self::ApplySettings => "apply_settings",
            Self::FactoryReset => "factory_reset",
            Self::FirmwareUpdate => "firmware_update",
            Self::ChangePassword => "change_password",
        }
    }

    pub fn irreversible(self) -> bool {
        matches!(self, Self::FactoryReset | Self::FirmwareUpdate | Self::ChangePassword)
    }

    /// Whether the action changes persistent settings (snapshot + rollback).
    pub fn changes_config(self) -> bool {
        matches!(self, Self::ApplySettings)
    }

    /// Phase-1 actions keep the pre-existing request default (false);
    /// everything newer is dry run unless explicitly turned off.
    pub fn default_dry_run(self) -> bool {
        !matches!(self, Self::Start | Self::Stop | Self::Reboot)
    }
}

impl From<MinerAction> for ActionKind {
    fn from(a: MinerAction) -> Self {
        match a {
            MinerAction::Start => Self::Start,
            MinerAction::Stop => Self::Stop,
            MinerAction::Reboot => Self::Reboot,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmation {
    /// What the operator typed into the confirmation box.
    #[serde(default)]
    pub typed: Option<String>,
    #[serde(default)]
    pub allow_irreversible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    IrreversibleNotAllowed,
    TypedConfirmationMismatch { expected: &'static str },
    IrreversibleBatch(usize),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IrreversibleNotAllowed => write!(f, "irreversible action: allow_irreversible is not set"),
            Self::TypedConfirmationMismatch { expected } => {
                write!(f, "irreversible action: type \"{expected}\" exactly to confirm")
            }
            Self::IrreversibleBatch(n) => write!(f, "irreversible action: one device at a time ({n} selected)"),
        }
    }
}

/// Pre-flight gate. A dry run never touches a device, so it always passes.
pub fn preflight(kind: ActionKind, ip_count: usize, dry_run: bool, c: &Confirmation) -> Result<(), Refusal> {
    if dry_run || !kind.irreversible() {
        return Ok(());
    }
    if !c.allow_irreversible {
        return Err(Refusal::IrreversibleNotAllowed);
    }
    if c.typed.as_deref() != Some(kind.name()) {
        return Err(Refusal::TypedConfirmationMismatch { expected: kind.name() });
    }
    if ip_count != 1 {
        return Err(Refusal::IrreversibleBatch(ip_count));
    }
    Ok(())
}
