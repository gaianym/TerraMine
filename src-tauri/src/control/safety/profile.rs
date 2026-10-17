//! Device identity used to pick write paths and limits.
//!
//! A [`DeviceProfile`] is built from whatever the driver or scan learned
//! (make / model / firmware / ASIC chip). [`DeviceClass`] is the coarse
//! family that decides which write paths are allowed to touch the device:
//! an AxeOS HTTP path must never talk to a cgminer device and vice versa.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceClass {
    /// Bitaxe / NerdAxe family running AxeOS (ESP-Miner) — HTTP JSON API.
    AxeOs,
    /// Bitmain stock firmware (CGI + cgminer API).
    AntminerStock,
    BraiinsOs,
    LuxOs,
    Vnish,
    Whatsminer,
    Avalon,
    Epic,
    Unknown,
}

impl DeviceClass {
    /// Families that speak the cgminer/bmminer JSON-RPC API on :4028.
    pub fn is_cgminer_family(self) -> bool {
        matches!(
            self,
            Self::AntminerStock | Self::BraiinsOs | Self::LuxOs | Self::Vnish | Self::Whatsminer | Self::Avalon
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceProfile {
    pub class: DeviceClass,
    pub make: String,
    pub model: String,
    pub firmware: String,
    /// ASIC chip (e.g. "BM1370"); AxeOS limits are keyed on this.
    pub chip: Option<String>,
}

impl DeviceProfile {
    pub fn unknown() -> Self {
        Self::classify("", "", "", None)
    }

    pub fn classify(make: &str, model: &str, firmware: &str, chip: Option<&str>) -> Self {
        Self {
            class: classify_class(make, model, firmware),
            make: make.to_string(),
            model: model.to_string(),
            firmware: firmware.to_string(),
            chip: chip.map(str::to_string),
        }
    }
}

/// Firmware wins over make: a BraiinsOS Antminer is not "stock".
pub fn classify_class(make: &str, model: &str, firmware: &str) -> DeviceClass {
    let make = make.to_ascii_lowercase();
    let model = model.to_ascii_lowercase();
    let fw = firmware.to_ascii_lowercase();
    if fw.contains("axeos") || fw.contains("esp-miner") || make.contains("bitaxe") || make.contains("nerdaxe") {
        return DeviceClass::AxeOs;
    }
    if fw.contains("braiins") || fw.contains("bos") {
        return DeviceClass::BraiinsOs;
    }
    if fw.contains("luxos") {
        return DeviceClass::LuxOs;
    }
    if fw.contains("vnish") {
        return DeviceClass::Vnish;
    }
    if fw.contains("epic") {
        return DeviceClass::Epic;
    }
    if make.contains("whatsminer") || make.contains("microbt") {
        return DeviceClass::Whatsminer;
    }
    if make.contains("avalon") || make.contains("canaan") {
        return DeviceClass::Avalon;
    }
    let bitmain = make.contains("antminer") || make.contains("bitmain") || model.starts_with("antminer");
    if bitmain && (fw.is_empty() || fw.contains("stock") || fw.contains("bitmain")) {
        return DeviceClass::AntminerStock;
    }
    DeviceClass::Unknown
}
