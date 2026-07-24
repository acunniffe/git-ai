//! SQLite access for git-ai's local databases.
//!
//! Each database keeps its own schema versioning and process-global singleton;
//! migrations are intentionally NOT consolidated across databases (e.g. the
//! metrics schema carries a partial index whose predicate is tied to a code
//! constant — merging version sequences would risk silent query breakage).
//!
//! - [`sqlite`] — shared connection helpers (memory limits, pragmas)
//! - [`metrics_db`] — metrics event store and upload queue
//! - [`notes_db`] — authorship notes (sqlite backend primary storage, HTTP
//!   queue, and read cache)
//! - [`internal_db`] — legacy prompts + CAS sync queue
//! - [`streams_db`] — transcript stream sessions and watermarks
//! - [`bash_history_db`] — bash tool-use checkpoint provenance
//! - [`lock_file`] — cross-platform advisory file locking (daemon lock)

pub mod bash_history_db;
pub mod checkpoint_outbox;
pub mod error;
pub mod internal_db;
pub mod lock_file;
pub mod metrics_db;
pub mod notes_db;
pub mod sqlite;
pub mod streams_db;
