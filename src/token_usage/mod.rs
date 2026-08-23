//! Token-usage extraction from agent transcripts.
//!
//! Computes per-session token usage and estimated cost in 5-minute UTC
//! buckets. The parsing and deduplication logic is ported from ccusage
//! (<https://github.com/ccusage/ccusage>, MIT License, Copyright (c) 2025
//! @ryoppippi) and adapted to git-ai's incremental transcript streaming:
//! whole-file passes become line-oriented extractors whose state persists
//! between runs, and deduplication moves into a database so entries can
//! stream in across runs. Per-extractor deviations are documented in
//! `claude.rs` and `codex.rs`.

pub mod claude;
pub mod codex;
pub mod cost;
pub mod extractor;
pub mod types;

pub use extractor::{UsageExtractor, extractor_for_tool};
pub use types::{TokenCounts, UsageEntry, bucket_ts};
