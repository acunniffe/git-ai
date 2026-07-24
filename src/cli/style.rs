//! Shared ANSI style constants for git-ai's human-facing terminal output.
//!
//! Only escape codes that genuinely recur byte-for-byte across call sites
//! live here. One-off styling (spinner frames, 256-color accents, progress
//! bar fills) stays local to its command module.

/// Dim/gray foreground (SGR 90).
pub(crate) const GRAY: &str = "\x1b[90m";
/// Bold (SGR 1).
pub(crate) const BOLD: &str = "\x1b[1m";
/// Reset all SGR attributes (SGR 0).
pub(crate) const RESET: &str = "\x1b[0m";
