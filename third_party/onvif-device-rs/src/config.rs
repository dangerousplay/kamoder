//! ONVIF device identity configuration (serde-friendly).
//!
//! TOML/JSON shapes are identical to the `mibee-eye-raspi-rs` `[device]`
//! section, so hosts can re-export [`DeviceConfig`] directly into their own
//! config structs without changing config files.
//!
//! Defaults are neutral placeholders ("unknown"); [`DeviceConfig::validate`]
//! rejects them so every host must configure its real identity (issue #20).

use serde::{Deserialize, Serialize};

/// ONVIF device information exposed via GetDeviceInformation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    #[serde(default = "default_device_name")]
    pub name: String,
    #[serde(default = "default_manufacturer")]
    pub manufacturer: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_firmware")]
    pub firmware: String,
    #[serde(default = "default_hardware_id")]
    pub hardware_id: String,
    #[serde(default)]
    pub serial_number: String,
}

fn default_device_name() -> String {
    "ONVIF Device".to_string()
}
fn default_manufacturer() -> String {
    "unknown".to_string()
}
fn default_model() -> String {
    "unknown".to_string()
}
fn default_firmware() -> String {
    "1.0.0".to_string()
}
fn default_hardware_id() -> String {
    "unknown".to_string()
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            name: default_device_name(),
            manufacturer: default_manufacturer(),
            model: default_model(),
            firmware: default_firmware(),
            hardware_id: default_hardware_id(),
            serial_number: String::new(),
        }
    }
}

/// Placeholder value marking an unconfigured identity field.
const NEUTRAL_PLACEHOLDER: &str = "unknown";

impl DeviceConfig {
    /// Validate the identity: every field is non-empty, and the neutral
    /// placeholders (`unknown`) are rejected — hosts must configure their
    /// real identity instead of leaking (or faking) someone else's
    /// hardware fingerprint (issue #20). The serial number may stay empty
    /// (a privacy-friendly host choice).
    pub fn validate(&self) -> Result<(), String> {
        for (field, value) in [
            ("name", &self.name),
            ("manufacturer", &self.manufacturer),
            ("model", &self.model),
            ("firmware", &self.firmware),
            ("hardware_id", &self.hardware_id),
        ] {
            if value.is_empty() {
                return Err(format!("device.{field} must not be empty"));
            }
            if field != "name" && value.as_str() == NEUTRAL_PLACEHOLDER {
                return Err(format!(
                    "device.{field} is the neutral placeholder \"{NEUTRAL_PLACEHOLDER}\" — configure the host's real identity"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Default` must stay in lockstep with the serde defaults so hosts
    /// constructing `DeviceConfig::default()` and hosts deserializing an
    /// empty `[device]` section see the same values.
    #[test]
    fn default_matches_serde_defaults() {
        let d = DeviceConfig::default();
        let s: DeviceConfig = toml::from_str("").expect("empty TOML deserializes");
        assert_eq!(d.name, s.name);
        assert_eq!(d.manufacturer, s.manufacturer);
        assert_eq!(d.model, s.model);
        assert_eq!(d.firmware, s.firmware);
        assert_eq!(d.hardware_id, s.hardware_id);
        assert_eq!(d.serial_number, s.serial_number);
    }

    #[test]
    fn empty_toml_section_yields_documented_defaults() {
        let c: DeviceConfig = toml::from_str("").unwrap();
        // Neutral placeholders — rejected by validate() until configured.
        assert_eq!(c.name, "ONVIF Device");
        assert_eq!(c.manufacturer, "unknown");
        assert_eq!(c.model, "unknown");
        assert_eq!(c.firmware, "1.0.0");
        assert_eq!(c.hardware_id, "unknown");
        assert_eq!(c.serial_number, "");
    }

    #[test]
    fn partial_toml_overrides_only_given_fields() {
        // The mibee-eye-raspi-rs `[device]` section sets exactly these.
        let c: DeviceConfig = toml::from_str(
            "\
             name = \"Desk Cam\"\n\
             model = \"IMX219\"\n\
             serial_number = \"SN-42\"\n\
             ",
        )
        .unwrap();
        assert_eq!(c.name, "Desk Cam");
        assert_eq!(c.model, "IMX219");
        assert_eq!(c.serial_number, "SN-42");
        // Untouched fields keep their defaults.
        assert_eq!(c.manufacturer, "unknown");
        assert_eq!(c.firmware, "1.0.0");
    }

    #[test]
    fn serialize_roundtrip_preserves_identity() {
        let c = DeviceConfig {
            name: "Notebook Cam".into(),
            manufacturer: "MiBee".into(),
            model: "Virtual".into(),
            firmware: "9.9.9".into(),
            hardware_id: "HW-X".into(),
            serial_number: "SER-1".into(),
        };
        let toml_str = toml::to_string(&c).unwrap();
        let back: DeviceConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(back.name, c.name);
        assert_eq!(back.manufacturer, c.manufacturer);
        assert_eq!(back.model, c.model);
        assert_eq!(back.firmware, c.firmware);
        assert_eq!(back.hardware_id, c.hardware_id);
        assert_eq!(back.serial_number, c.serial_number);
    }

    /// Issue #20: defaults must not fingerprint the origin hardware — a
    /// library default advertising "Raspberry Pi / OV5647" on every
    /// non-Pi host is leaked branding.
    #[test]
    fn defaults_are_neutral() {
        let d = DeviceConfig::default();
        assert_eq!(d.manufacturer, "unknown");
        assert_eq!(d.model, "unknown");
        assert_eq!(d.hardware_id, "unknown");
        assert_ne!(d.name, "Pi Camera V1");
    }

    /// Issue #20: unset identity (the neutral placeholders) must be
    /// rejected — hosts are required to configure their real identity.
    #[test]
    fn validate_rejects_neutral_placeholders() {
        let err = DeviceConfig::default().validate().unwrap_err();
        assert!(err.contains("manufacturer"), "got: {err}");

        let c = DeviceConfig {
            manufacturer: "MiBee".into(),
            ..DeviceConfig::default()
        };
        let err = c.validate().unwrap_err();
        assert!(err.contains("model"), "got: {err}");
    }

    #[test]
    fn validate_rejects_empty_fields() {
        let mut c = explicit_config();
        c.name = String::new();
        assert!(c.validate().unwrap_err().contains("name"));

        let mut c = explicit_config();
        c.firmware = String::new();
        assert!(c.validate().unwrap_err().contains("firmware"));

        // Serial number may stay empty (privacy-friendly host choice).
        let mut c = explicit_config();
        c.serial_number = String::new();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn validate_accepts_explicit_identity() {
        assert!(explicit_config().validate().is_ok());
    }

    fn explicit_config() -> DeviceConfig {
        DeviceConfig {
            name: "Desk Cam".into(),
            manufacturer: "MiBee".into(),
            model: "IMX219".into(),
            firmware: "1.0.0".into(),
            hardware_id: "HW-1".into(),
            serial_number: "SN-1".into(),
        }
    }
}
