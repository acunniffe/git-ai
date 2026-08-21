//! Reset locally stored metric events so the daemon delivers them again.

use crate::daemon::{ControlRequest, send_control_request};
use chrono::DateTime;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReingestScope {
    from_ts: Option<u32>,
    to_ts: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ReingestResponse {
    reset: usize,
}

pub(crate) fn handle_reingest(args: &[String]) {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
    {
        print_usage();
        return;
    }

    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(u64::from(u32::MAX)) as u32;
    let scope = match parse_reingest_scope(args, now_ts) {
        Ok(scope) => scope,
        Err(error) => {
            eprintln!("reingest: {error}");
            print_usage();
            std::process::exit(1);
        }
    };

    let config = match crate::commands::daemon::ensure_daemon_running(Duration::from_secs(5)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("reingest: failed to reach git-ai background service: {error}");
            std::process::exit(1);
        }
    };
    let request = ControlRequest::ReingestMetrics {
        from_ts: scope.from_ts,
        to_ts: scope.to_ts,
    };
    let response = match send_control_request(&config.control_socket_path, &request) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("reingest: failed to send request to background service: {error}");
            std::process::exit(1);
        }
    };
    if !response.ok {
        eprintln!(
            "reingest: {}",
            response
                .error
                .as_deref()
                .unwrap_or("background service returned an error")
        );
        std::process::exit(1);
    }

    let result = response
        .data
        .and_then(|value| serde_json::from_value::<ReingestResponse>(value).ok())
        .unwrap_or_else(|| {
            eprintln!("reingest: background service returned an invalid response");
            std::process::exit(1);
        });
    println!(
        "reingest: reset {} metric event(s); delivery will continue in the background",
        result.reset
    );
}

fn parse_reingest_scope(args: &[String], now_ts: u32) -> Result<ReingestScope, String> {
    let mut all = false;
    let mut from = None;
    let mut to = None;
    let mut since = None;
    let mut index = 0;

    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--all" if !all => {
                all = true;
                index += 1;
            }
            "--all" => return Err("--all may only be specified once".to_string()),
            "--from" | "--to" | "--since" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                let destination = match flag {
                    "--from" => &mut from,
                    "--to" => &mut to,
                    _ => &mut since,
                };
                if destination.replace(value.clone()).is_some() {
                    return Err(format!("{flag} may only be specified once"));
                }
                index += 2;
            }
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }

    if all {
        if from.is_some() || to.is_some() || since.is_some() {
            return Err("--all cannot be combined with time bounds".to_string());
        }
        return Ok(ReingestScope {
            from_ts: None,
            to_ts: None,
        });
    }

    if let Some(since) = since {
        if from.is_some() || to.is_some() {
            return Err("--since cannot be combined with --from or --to".to_string());
        }
        let duration = parse_since_duration(&since)?;
        return Ok(ReingestScope {
            from_ts: Some(now_ts.saturating_sub(duration.min(u64::from(u32::MAX)) as u32)),
            to_ts: Some(now_ts),
        });
    }

    let (Some(from), Some(to)) = (from, to) else {
        return Err("use --all, --since, or both --from and --to".to_string());
    };
    let from_ts = parse_rfc3339_timestamp("--from", &from)?;
    let to_ts = parse_rfc3339_timestamp("--to", &to)?;
    if from_ts >= to_ts {
        return Err("--from must be earlier than --to".to_string());
    }
    Ok(ReingestScope {
        from_ts: Some(from_ts),
        to_ts: Some(to_ts),
    })
}

fn parse_rfc3339_timestamp(flag: &str, value: &str) -> Result<u32, String> {
    let timestamp = DateTime::parse_from_rfc3339(value)
        .map_err(|_| format!("{flag} must be an RFC3339 timestamp"))?
        .timestamp();
    u32::try_from(timestamp)
        .map_err(|_| format!("{flag} is outside the supported metric timestamp range"))
}

fn parse_since_duration(value: &str) -> Result<u64, String> {
    let split_index = value
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index);
    let (digits, unit) = value.split_at(split_index);
    let amount = digits
        .parse::<u64>()
        .ok()
        .filter(|amount| *amount > 0)
        .ok_or_else(|| {
            "--since must be a positive integer followed by s, m, h, d, or w".to_string()
        })?;
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => {
            return Err(
                "--since must be a positive integer followed by s, m, h, d, or w".to_string(),
            );
        }
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| "--since duration is too large".to_string())
}

fn print_usage() {
    eprintln!("Usage:");
    eprintln!("  git-ai reingest --all");
    eprintln!("  git-ai reingest --from <RFC3339> --to <RFC3339>");
    eprintln!("  git-ai reingest --since <duration>");
    eprintln!();
    eprintln!("Reset local metric delivery state and let the daemon redeliver matching events.");
    eprintln!("Explicit bounds use a half-open [from, to) event-time range.");
    eprintln!("Durations are positive integers with one of: s, m, h, d, w.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn parses_all_time_scope() {
        assert_eq!(
            parse_reingest_scope(&args(&["--all"]), 1_700_000_000).unwrap(),
            ReingestScope {
                from_ts: None,
                to_ts: None,
            }
        );
    }

    #[test]
    fn parses_half_open_rfc3339_scope_with_offsets() {
        assert_eq!(
            parse_reingest_scope(
                &args(&[
                    "--from",
                    "2023-11-14T17:13:20-05:00",
                    "--to",
                    "2023-11-14T23:13:20+00:00",
                ]),
                1_700_000_000,
            )
            .unwrap(),
            ReingestScope {
                from_ts: Some(1_700_000_000),
                to_ts: Some(1_700_003_600),
            }
        );
    }

    #[test]
    fn parses_supported_since_units() {
        for (value, seconds) in [
            ("30s", 30),
            ("5m", 300),
            ("2h", 7_200),
            ("7d", 604_800),
            ("2w", 1_209_600),
        ] {
            assert_eq!(
                parse_reingest_scope(&args(&["--since", value]), 1_700_000_000).unwrap(),
                ReingestScope {
                    from_ts: Some(1_700_000_000 - seconds),
                    to_ts: Some(1_700_000_000),
                },
                "failed to parse {value}"
            );
        }
    }

    #[test]
    fn rejects_missing_mixed_or_invalid_scopes() {
        for invalid in [
            vec![],
            vec!["--from", "2023-11-14T22:13:20Z"],
            vec!["--to", "2023-11-14T22:13:20Z"],
            vec!["--all", "--since", "1d"],
            vec!["--since", "0d"],
            vec!["--since", "1d2h"],
            vec!["--since", "5µ"],
            vec![
                "--from",
                "2023-11-14T22:13:20Z",
                "--to",
                "2023-11-14T22:13:20Z",
            ],
            vec!["unexpected"],
        ] {
            assert!(
                parse_reingest_scope(&args(&invalid), 1_700_000_000).is_err(),
                "unexpectedly accepted {invalid:?}"
            );
        }
    }
}
