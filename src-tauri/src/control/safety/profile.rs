//! Device identity for the safety layer: a thin mapping from what
//! discovery / asic-rs already reported (make, model, firmware, chip) to
//! the coarse [`DeviceClass`] used for limits and audit. No probing here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceClass {
    AxeOs,
    AntminerStock,
    BraiinsOs,
    LuxOs,
    Vnish,
    Whatsminer,
    Avalon,
    Epic,
    Proto,
    Sealminer,
    Auradine,
    Unknown,
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

/// (field, needle, class) — first match wins; firmware rules come first so
/// a BraiinsOS Antminer is not "stock".
const RULES: &[(Field, &str, DeviceClass)] = &[
    (Field::Firmware, "axeos", DeviceClass::AxeOs),
    (Field::Make, "bitaxe", DeviceClass::AxeOs),
    (Field::Make, "nerdaxe", DeviceClass::AxeOs),
    (Field::Firmware, "braiins", DeviceClass::BraiinsOs),
    (Field::Firmware, "luxos", DeviceClass::LuxOs),
    (Field::Firmware, "vnish", DeviceClass::Vnish),
    (Field::Firmware, "epic", DeviceClass::Epic),
    (Field::Make, "whatsminer", DeviceClass::Whatsminer),
    (Field::Make, "avalon", DeviceClass::Avalon),
    (Field::Make, "proto", DeviceClass::Proto),
    (Field::Make, "sealminer", DeviceClass::Sealminer),
    (Field::Make, "auradine", DeviceClass::Auradine),
    (Field::Make, "antminer", DeviceClass::AntminerStock),
];

#[derive(Clone, Copy)]
enum Field {
    Make,
    Firmware,
}

pub fn classify_class(make: &str, firmware: &str) -> DeviceClass {
    let (make, fw) = (make.to_ascii_lowercase(), firmware.to_ascii_lowercase());
    RULES
        .iter()
        .find(|(f, needle, _)| match f {
            Field::Make => make.contains(needle),
            Field::Firmware => fw.contains(needle),
        })
        .map_or(DeviceClass::Unknown, |r| r.2)
}

impl DeviceProfile {
    pub fn unknown() -> Self {
        Self::classify("", "", "", None)
    }

    pub fn classify(make: &str, model: &str, firmware: &str, chip: Option<&str>) -> Self {
        Self {
            class: classify_class(make, firmware),
            make: make.into(),
            model: model.into(),
            firmware: firmware.into(),
            chip: chip.map(Into::into),
        }
    }
}
