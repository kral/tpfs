//! # tpfs-core
//!
//! Every change to the file system is first described as a [`Plan`] — an ordered list of [`Op`]s.
//! A plan is [validated](Validator) against a set of allowed roots, assessed for [risk](risk),
//! shown to a human, and only then [executed](Executor). Execution is recorded in a write-ahead
//! [journal](Journal), so every committed batch can be undone and a batch interrupted by a crash
//! can be rolled back on the next start.
//!
//! There is deliberately no permanent delete: `trash` moves into a journaled trash directory.

mod display;
mod executor;
mod fsops;
mod journal;
mod op;
mod path;
pub mod risk;
pub mod sync;
mod validate;

pub use display::sanitize;
pub use executor::{BatchReport, ExecError, Executor, Phase, RecoveryReport, Source};
pub use journal::{BatchStatus, BatchView, Entry, Journal, Step};
pub use op::{Op, Plan};
pub use path::{canonicalize, check_private_file, normalize, private_dir, share_root, system_roots};
pub use validate::{Issue, MAX_OPS, MAX_PATH, MAX_REASON, Validator};
