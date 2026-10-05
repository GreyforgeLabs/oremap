//! Read-only workflow inventory for local automation surfaces.
//!
//! `oremap` inventories systemd units, cron entries, wrapper
//! scripts, Git repositories, and env-file metadata, then produces a
//! conservative keep/review/retire matrix. It never starts, stops, enables,
//! deletes, or edits anything, and it never emits env-file values.

pub mod argparse;
pub mod classifier;
pub mod cli;
pub mod collectors;
pub mod formatters;
pub mod models;
pub mod paths;
pub mod privacy;
pub mod pyfmt;
pub mod timefmt;

pub use cli::VERSION;
