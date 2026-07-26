//! Test-support projection between runtime overrides and on-disk config.

use super::file::{ConfigPatch, FileConfig};

impl ConfigPatch {
    /// Project test overrides into the on-disk config schema used by dedicated
    /// test daemons.
    pub fn to_file_config(&self) -> Result<FileConfig, serde_json::Error> {
        let mut value = serde_json::to_value(self)?;
        let fields = value
            .as_object_mut()
            .expect("ConfigPatch should serialize as a JSON object");

        fields.remove("telemetry_oss_disabled");
        if let Some(disabled) = self.telemetry_oss_disabled {
            fields.insert(
                "telemetry_oss".to_string(),
                serde_json::Value::String(if disabled { "off" } else { "on" }.to_string()),
            );
        }

        serde_json::from_value(value)
    }
}
