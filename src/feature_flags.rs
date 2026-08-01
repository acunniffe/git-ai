use serde::{Deserialize, Serialize};

/// Parse a boolean from an environment variable string.
/// Accepts: "1", "true", "True", "TRUE" → Some(true)
///          "0", "false", "False", "FALSE" → Some(false)
///          Missing or invalid → None
fn parse_bool_env(value: Option<String>) -> Option<bool> {
    value.and_then(|v| match v.as_str() {
        "1" | "true" | "True" | "TRUE" => Some(true),
        "0" | "false" | "False" | "FALSE" => Some(false),
        _ => None,
    })
}

macro_rules! define_feature_flags {
    (
        $(
            $field:ident: $file_name:ident, debug = $debug_default:expr, release = $release_default:expr
        ),* $(,)?
    ) => {
        /// Feature flags for the application
        #[derive(Debug, Clone, Serialize)]
        pub struct FeatureFlags {
            $(pub $field: bool,)*
        }

        impl Default for FeatureFlags {
            fn default() -> Self {
                #[cfg(debug_assertions)]
                {
                    return FeatureFlags {
                        $($field: $debug_default,)*
                    };
                }
                #[cfg(not(debug_assertions))]
                FeatureFlags {
                    $($field: $release_default,)*
                }
            }
        }

        /// Deserializable version of FeatureFlags with all optional fields
        /// Works for both file config and environment variables
        #[derive(Deserialize, Default)]
        #[serde(default)]
        pub(crate) struct DeserializableFeatureFlags {
            $(
                #[serde(default)]
                $file_name: Option<bool>,
            )*
        }

        impl DeserializableFeatureFlags {
            /// Parse feature flags from environment variables with the given prefix.
            /// Converts field names to SCREAMING_SNAKE_CASE.
            pub(crate) fn from_env(prefix: &str) -> Self {
                DeserializableFeatureFlags {
                    $(
                        $file_name: parse_bool_env(
                            std::env::var(format!("{}{}", prefix, stringify!($file_name).to_uppercase())).ok()
                        ),
                    )*
                }
            }
        }

        impl FeatureFlags {
            /// Merge flags with a base, applying any Some values as overrides
            pub(crate) fn merge_with(base: Self, overrides: DeserializableFeatureFlags) -> Self {
                FeatureFlags {
                    $($field: overrides.$file_name.unwrap_or(base.$field),)*
                }
            }
        }
    };
}

// Define all feature flags in one place
// Format: struct_field: file_and_env_name, debug = <bool>, release = <bool>
define_feature_flags!(
    lite_mode: lite_mode, debug = false, release = false,
    auth_keyring: auth_keyring, debug = false, release = false,
    transcript_streaming: transcript_streaming, debug = true, release = true,
    transcript_sweep: transcript_sweep, debug = true, release = true,
    checkpoint_debug_log: checkpoint_debug_log, debug = false, release = false,
    bash_checkpoints_v2: bash_checkpoints_v2, debug = false, release = false,
    daemon_log_upload: daemon_log_upload, debug = true, release = true,
    rewrite_metrics_events: rewrite_metrics_events, debug = true, release = false,
);

impl FeatureFlags {
    /// Build FeatureFlags from deserializable config
    fn from_deserializable(flags: DeserializableFeatureFlags) -> Self {
        Self::merge_with(FeatureFlags::default(), flags)
    }

    /// Build FeatureFlags from environment variables
    /// Reads from GIT_AI_* prefixed environment variables
    /// Example: GIT_AI_AUTH_KEYRING=true
    /// Falls back to defaults for any invalid or missing values
    #[allow(dead_code)]
    pub fn from_env() -> Self {
        let env_flags = DeserializableFeatureFlags::from_env("GIT_AI_");
        Self::from_deserializable(env_flags)
    }

    /// Build FeatureFlags from both file and environment variables
    /// Precedence: Environment > File > Default
    /// - Starts with defaults
    /// - Applies file config overrides if present
    /// - Applies environment variable overrides if present (highest priority)
    pub(crate) fn from_env_and_file(file_flags: Option<DeserializableFeatureFlags>) -> Self {
        // Start with defaults
        let mut result = FeatureFlags::default();

        // Apply file config overrides
        if let Some(file) = file_flags {
            result = Self::merge_with(result, file);
        }

        // Apply env var overrides (highest priority)
        let env_flags = DeserializableFeatureFlags::from_env("GIT_AI_");
        result = Self::merge_with(result, env_flags);

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_feature_flags() {
        let flags = FeatureFlags::default();
        #[cfg(debug_assertions)]
        {
            assert!(!flags.lite_mode);
            assert!(!flags.auth_keyring);
            assert!(flags.transcript_streaming);
            assert!(flags.transcript_sweep);
            assert!(!flags.checkpoint_debug_log);
            assert!(!flags.bash_checkpoints_v2);
            assert!(flags.daemon_log_upload);
            assert!(flags.rewrite_metrics_events);
        }
        #[cfg(not(debug_assertions))]
        {
            assert!(!flags.lite_mode);
            assert!(!flags.auth_keyring);
            assert!(flags.transcript_streaming);
            assert!(flags.transcript_sweep);
            assert!(!flags.checkpoint_debug_log);
            assert!(!flags.bash_checkpoints_v2);
            assert!(flags.daemon_log_upload);
            assert!(!flags.rewrite_metrics_events);
        }
    }

    #[test]
    fn test_lite_mode_file_override() {
        let deserializable = DeserializableFeatureFlags {
            lite_mode: Some(true),
            ..Default::default()
        };

        let flags = FeatureFlags::from_deserializable(deserializable);
        assert!(flags.lite_mode);
    }

    #[test]
    #[serial_test::serial]
    fn test_lite_mode_env_overrides_file() {
        unsafe {
            std::env::set_var("GIT_AI_LITE_MODE", "true");
        }

        let file_flags = DeserializableFeatureFlags {
            lite_mode: Some(false),
            ..Default::default()
        };

        let flags = FeatureFlags::from_env_and_file(Some(file_flags));
        assert!(flags.lite_mode);

        unsafe {
            std::env::remove_var("GIT_AI_LITE_MODE");
        }
    }

    #[test]
    fn test_from_deserializable() {
        let deserializable = DeserializableFeatureFlags {
            auth_keyring: Some(true),
            ..Default::default()
        };

        let flags = FeatureFlags::from_deserializable(deserializable);
        assert!(flags.auth_keyring);
    }

    #[test]
    fn test_rewrite_metrics_events_file_override() {
        let deserializable = DeserializableFeatureFlags {
            rewrite_metrics_events: Some(false),
            ..Default::default()
        };

        let flags = FeatureFlags::from_deserializable(deserializable);
        assert!(!flags.rewrite_metrics_events);
    }

    #[test]
    #[serial_test::serial]
    fn test_from_env_and_file_defaults_only() {
        unsafe {
            std::env::remove_var("GIT_AI_AUTH_KEYRING");
            std::env::remove_var("GIT_AI_REWRITE_METRICS_EVENTS");
        }

        let flags = FeatureFlags::from_env_and_file(None);
        let defaults = FeatureFlags::default();
        assert_eq!(flags.auth_keyring, defaults.auth_keyring);
        assert_eq!(
            flags.rewrite_metrics_events,
            defaults.rewrite_metrics_events
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_from_env_and_file_file_overrides() {
        unsafe {
            std::env::remove_var("GIT_AI_AUTH_KEYRING");
            std::env::remove_var("GIT_AI_REWRITE_METRICS_EVENTS");
        }

        let file_flags = DeserializableFeatureFlags {
            auth_keyring: Some(true),
            ..Default::default()
        };

        let flags = FeatureFlags::from_env_and_file(Some(file_flags));
        assert!(flags.auth_keyring);
    }

    #[test]
    #[serial_test::serial]
    fn test_rewrite_metrics_events_env_overrides_file() {
        unsafe {
            std::env::set_var("GIT_AI_REWRITE_METRICS_EVENTS", "true");
        }

        let file_flags = DeserializableFeatureFlags {
            rewrite_metrics_events: Some(false),
            ..Default::default()
        };

        let flags = FeatureFlags::from_env_and_file(Some(file_flags));
        assert!(flags.rewrite_metrics_events);

        unsafe {
            std::env::remove_var("GIT_AI_REWRITE_METRICS_EVENTS");
        }
    }

    #[test]
    fn test_serialization() {
        let flags = FeatureFlags {
            lite_mode: true,
            auth_keyring: true,
            transcript_streaming: true,
            transcript_sweep: true,
            checkpoint_debug_log: false,
            bash_checkpoints_v2: true,
            daemon_log_upload: true,
            rewrite_metrics_events: true,
        };

        let serialized = serde_json::to_string(&flags).unwrap();
        assert!(serialized.contains("lite_mode"));
        assert!(serialized.contains("auth_keyring"));
        assert!(serialized.contains("transcript_streaming"));
        assert!(serialized.contains("transcript_sweep"));
        assert!(serialized.contains("checkpoint_debug_log"));
        assert!(serialized.contains("bash_checkpoints_v2"));
        assert!(serialized.contains("daemon_log_upload"));
        assert!(serialized.contains("rewrite_metrics_events"));
    }

    #[test]
    fn test_clone_trait() {
        let flags = FeatureFlags {
            lite_mode: true,
            auth_keyring: true,
            transcript_streaming: true,
            transcript_sweep: true,
            checkpoint_debug_log: true,
            bash_checkpoints_v2: true,
            daemon_log_upload: true,
            rewrite_metrics_events: true,
        };
        let cloned = flags.clone();
        assert_eq!(cloned.lite_mode, flags.lite_mode);
        assert_eq!(cloned.auth_keyring, flags.auth_keyring);
        assert_eq!(cloned.transcript_streaming, flags.transcript_streaming);
        assert_eq!(cloned.transcript_sweep, flags.transcript_sweep);
        assert_eq!(cloned.checkpoint_debug_log, flags.checkpoint_debug_log);
        assert_eq!(cloned.bash_checkpoints_v2, flags.bash_checkpoints_v2);
        assert_eq!(cloned.daemon_log_upload, flags.daemon_log_upload);
        assert_eq!(cloned.rewrite_metrics_events, flags.rewrite_metrics_events);
    }

    #[test]
    fn test_debug_trait() {
        let flags = FeatureFlags::default();
        let debug_str = format!("{:?}", flags);
        assert!(debug_str.contains("FeatureFlags"));
    }
}
