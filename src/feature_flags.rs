use serde::{Deserialize, Serialize};

pub const DEFAULT_HOOKS_SYNC_INTERVAL_SECS: u64 = 6 * 60 * 60;

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

fn parse_u64_env(value: Option<String>) -> Option<u64> {
    value.and_then(|value| value.parse().ok())
}

macro_rules! define_feature_flags {
    (
        $(
            $field:ident: $file_name:ident ($type:ty, $env_parser:ident), debug = $debug_default:expr, release = $release_default:expr
        ),* $(,)?
    ) => {
        /// Feature flags for the application
        #[derive(Debug, Clone, Serialize)]
        pub struct FeatureFlags {
            $(pub $field: $type,)*
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
                $file_name: Option<$type>,
            )*
        }

        impl DeserializableFeatureFlags {
            /// Parse feature flags from environment variables with the given prefix.
            /// Converts field names to SCREAMING_SNAKE_CASE.
            pub(crate) fn from_env(prefix: &str) -> Self {
                DeserializableFeatureFlags {
                    $(
                        $file_name: $env_parser(
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
// Format: struct_field: file_and_env_name (type, env_parser), debug = <value>, release = <value>
define_feature_flags!(
    auth_keyring: auth_keyring (bool, parse_bool_env), debug = false, release = false,
    transcript_streaming: transcript_streaming (bool, parse_bool_env), debug = true, release = true,
    transcript_sweep: transcript_sweep (bool, parse_bool_env), debug = true, release = true,
    checkpoint_debug_log: checkpoint_debug_log (bool, parse_bool_env), debug = false, release = false,
    bash_checkpoints_v2: bash_checkpoints_v2 (bool, parse_bool_env), debug = false, release = false,
    daemon_log_upload: daemon_log_upload (bool, parse_bool_env), debug = true, release = true,
    rewrite_metrics_events: rewrite_metrics_events (bool, parse_bool_env), debug = true, release = false,
    // Interval is expressed in seconds. Zero disables automatic hook synchronization.
    hooks_sync_interval: hooks_sync_interval (u64, parse_u64_env), debug = DEFAULT_HOOKS_SYNC_INTERVAL_SECS, release = DEFAULT_HOOKS_SYNC_INTERVAL_SECS,
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
        assert_eq!(flags.hooks_sync_interval, DEFAULT_HOOKS_SYNC_INTERVAL_SECS);
        #[cfg(debug_assertions)]
        {
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
    fn test_hooks_sync_interval_file_override() {
        let flags = FeatureFlags::from_deserializable(DeserializableFeatureFlags {
            hooks_sync_interval: Some(90),
            ..Default::default()
        });

        assert_eq!(flags.hooks_sync_interval, 90);
    }

    #[test]
    #[serial_test::serial]
    fn test_hooks_sync_interval_env_overrides_file() {
        unsafe {
            std::env::set_var("GIT_AI_HOOKS_SYNC_INTERVAL", "45");
        }

        let flags = FeatureFlags::from_env_and_file(Some(DeserializableFeatureFlags {
            hooks_sync_interval: Some(90),
            ..Default::default()
        }));

        unsafe {
            std::env::remove_var("GIT_AI_HOOKS_SYNC_INTERVAL");
        }
        assert_eq!(flags.hooks_sync_interval, 45);
    }

    #[test]
    #[serial_test::serial]
    fn test_invalid_hooks_sync_interval_env_uses_file_value() {
        unsafe {
            std::env::set_var("GIT_AI_HOOKS_SYNC_INTERVAL", "not-a-number");
        }

        let flags = FeatureFlags::from_env_and_file(Some(DeserializableFeatureFlags {
            hooks_sync_interval: Some(90),
            ..Default::default()
        }));

        unsafe {
            std::env::remove_var("GIT_AI_HOOKS_SYNC_INTERVAL");
        }
        assert_eq!(flags.hooks_sync_interval, 90);
    }

    #[test]
    fn test_serialization() {
        let flags = FeatureFlags {
            auth_keyring: true,
            transcript_streaming: true,
            transcript_sweep: true,
            checkpoint_debug_log: false,
            bash_checkpoints_v2: true,
            daemon_log_upload: true,
            rewrite_metrics_events: true,
            hooks_sync_interval: DEFAULT_HOOKS_SYNC_INTERVAL_SECS,
        };

        let serialized = serde_json::to_string(&flags).unwrap();
        assert!(serialized.contains("auth_keyring"));
        assert!(serialized.contains("transcript_streaming"));
        assert!(serialized.contains("transcript_sweep"));
        assert!(serialized.contains("checkpoint_debug_log"));
        assert!(serialized.contains("bash_checkpoints_v2"));
        assert!(serialized.contains("daemon_log_upload"));
        assert!(serialized.contains("rewrite_metrics_events"));
        assert!(serialized.contains("hooks_sync_interval"));
    }

    #[test]
    fn test_clone_trait() {
        let flags = FeatureFlags {
            auth_keyring: true,
            transcript_streaming: true,
            transcript_sweep: true,
            checkpoint_debug_log: true,
            bash_checkpoints_v2: true,
            daemon_log_upload: true,
            rewrite_metrics_events: true,
            hooks_sync_interval: DEFAULT_HOOKS_SYNC_INTERVAL_SECS,
        };
        let cloned = flags.clone();
        assert_eq!(cloned.auth_keyring, flags.auth_keyring);
        assert_eq!(cloned.transcript_streaming, flags.transcript_streaming);
        assert_eq!(cloned.transcript_sweep, flags.transcript_sweep);
        assert_eq!(cloned.checkpoint_debug_log, flags.checkpoint_debug_log);
        assert_eq!(cloned.bash_checkpoints_v2, flags.bash_checkpoints_v2);
        assert_eq!(cloned.daemon_log_upload, flags.daemon_log_upload);
        assert_eq!(cloned.rewrite_metrics_events, flags.rewrite_metrics_events);
        assert_eq!(cloned.hooks_sync_interval, flags.hooks_sync_interval);
    }

    #[test]
    fn test_debug_trait() {
        let flags = FeatureFlags::default();
        let debug_str = format!("{:?}", flags);
        assert!(debug_str.contains("FeatureFlags"));
    }
}
