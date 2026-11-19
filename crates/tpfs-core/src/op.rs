use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::display::sanitize;

/// One file operation. Paths are absolute; `to` is the full destination path, not a folder to drop into.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
#[non_exhaustive]
pub enum Op {
	/// Move a file or directory.
	Move { from: PathBuf, to: PathBuf },
	/// Copy a file or directory (recursively). Never overwrites.
	Copy { from: PathBuf, to: PathBuf },
	/// Rename in place. `newName` is a bare file name.
	Rename {
		path: PathBuf,
		#[serde(rename = "newName")]
		new_name: String,
	},
	/// Move to the journaled trash (undoable). There is no permanent delete.
	Trash { path: PathBuf },
	/// Create a directory.
	Mkdir { path: PathBuf },
	/// Remove an empty directory.
	Rmdir { path: PathBuf },
}

/// An ordered list of operations plus a short human-readable reason.
/// Unknown fields are rejected: an agent that invents an option such as `force` must learn that it
/// does not exist, instead of believing it took effect.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Plan {
	pub ops: Vec<Op>,
	#[serde(default)]
	pub reason: String,
}

impl Op {
	/// The paths this operation names (for `rename`, the path being renamed).
	pub fn paths(&self) -> Vec<&std::path::Path> {
		match self {
			Op::Move { from, to } | Op::Copy { from, to } => vec![from, to],
			Op::Rename { path, .. } | Op::Trash { path } | Op::Mkdir { path } | Op::Rmdir { path } => vec![path],
		}
	}
}

impl Plan {
	pub fn new(ops: Vec<Op>, reason: impl Into<String>) -> Self {
		Self { ops, reason: reason.into() }
	}

	/// SHA-256 of the canonical JSON form. An approval refers to exactly this plan and nothing else.
	pub fn digest(&self) -> String {
		// A path that is not valid UTF-8 has no JSON form; validation rejects such plans, and the
		// lossless Debug form still gives each of them a distinct digest instead of a panic.
		let bytes = serde_json::to_vec(self).unwrap_or_else(|_| format!("{self:?}").into_bytes());
		Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
	}

	#[cfg(feature = "schema")]
	/// JSON Schema of a plan, generated from the Rust types so prompts and code never drift apart.
	pub fn schema() -> serde_json::Value {
		serde_json::to_value(schemars::schema_for!(Plan)).expect("schema serialises")
	}

	/// Human-readable preview lines with all paths sanitised for safe display.
	/// One line per operation, with paths as they will be acted on (`..` and `.` resolved), so a
	/// preview cannot read differently from what happens.
	pub fn preview(&self) -> Vec<String> {
		self.normalized().ops.iter().map(ToString::to_string).collect()
	}

	/// The same plan with every absolute path lexically normalised (relative ones are left alone:
	/// validation rejects them).
	pub fn normalized(&self) -> Plan {
		let n = |p: &PathBuf| crate::path::normalize(p).unwrap_or_else(|| p.clone());
		let ops = self
			.ops
			.iter()
			.map(|op| match op {
				Op::Move { from, to } => Op::Move { from: n(from), to: n(to) },
				Op::Copy { from, to } => Op::Copy { from: n(from), to: n(to) },
				Op::Rename { path, new_name } => Op::Rename { path: n(path), new_name: new_name.clone() },
				Op::Trash { path } => Op::Trash { path: n(path) },
				Op::Mkdir { path } => Op::Mkdir { path: n(path) },
				Op::Rmdir { path } => Op::Rmdir { path: n(path) },
			})
			.collect();
		Plan { ops, reason: self.reason.clone() }
	}
}

impl fmt::Display for Op {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let s = |p: &PathBuf| sanitize(p.as_os_str());
		match self {
			Op::Move { from, to } => write!(f, "MOVE    {}  ->  {}", s(from), s(to)),
			Op::Copy { from, to } => write!(f, "COPY    {}  ->  {}", s(from), s(to)),
			Op::Rename { path, new_name } => {
				write!(f, "RENAME  {}  ->  {}", s(path), sanitize(new_name.as_ref()))
			}
			Op::Trash { path } => write!(f, "TRASH   {}", s(path)),
			Op::Mkdir { path } => write!(f, "MKDIR   {}", s(path)),
			Op::Rmdir { path } => write!(f, "RMDIR   {}", s(path)),
		}
	}
}
