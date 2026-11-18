//! Risk assessment of a plan, computed by the engine — never by whoever proposed the plan.
//!
//! The level decides how the plan is previewed: `Low` can be confirmed inline (a routine F5),
//! `Medium` gets a summary, `High` requires reading the full list.

use std::{fs, path::Path};

use crate::{
	executor::Source,
	op::{Op, Plan},
	path::normalize,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
	Low,
	Medium,
	High,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Risk {
	pub level: Level,
	pub reasons: Vec<String>,
}

pub fn assess(plan: &Plan, source: Source) -> Risk {
	let mut level = Level::Low;
	let mut reasons = Vec::new();
	let mut raise = |l: Level, why: String| {
		level = level.max(l);
		reasons.push(why);
	};

	if source == Source::Agent {
		raise(Level::High, "proposed by an agent".into());
	}

	match plan.ops.len() {
		n if n > 200 => raise(Level::High, format!("{n} operations")),
		n if n > 20 => raise(Level::Medium, format!("{n} operations")),
		_ => {}
	}

	let trashed_dirs = plan.ops.iter().filter(|op| matches!(op, Op::Trash { path } if is_dir(path))).count();
	let trashed = plan.ops.iter().filter(|op| matches!(op, Op::Trash { .. })).count();
	if trashed_dirs > 0 {
		raise(Level::High, format!("moves {trashed_dirs} whole folder(s) to the trash"));
	} else if trashed > 10 {
		raise(Level::Medium, format!("moves {trashed} files to the trash"));
	}

	// Trash a file, then put something else at the same path: in effect a replacement (undoable,
	// but the user should know the current version is being swapped out).
	// One pass to note where each path is last written, so a 500 000-step plan stays linear.
	let mut last_write: std::collections::HashMap<&Path, usize> = std::collections::HashMap::new();
	for (i, op) in plan.ops.iter().enumerate() {
		if let Op::Copy { to, .. } | Op::Move { to, .. } = op {
			last_write.insert(to.as_path(), i);
		}
	}
	let replaced = plan
		.ops
		.iter()
		.enumerate()
		.filter(|(i, op)| matches!(op, Op::Trash { path } if last_write.get(path.as_path()).is_some_and(|w| w > i)))
		.count();
	if replaced > 0 {
		raise(Level::Medium, format!("replaces {replaced} existing file(s); the current versions go to the trash"));
	}

	let linked: Vec<u64> = plan
		.ops
		.iter()
		.filter_map(|op| match op {
			Op::Trash { path } => hard_links(path),
			_ => None,
		})
		.collect();
	if !linked.is_empty() {
		raise(
			Level::Medium,
			format!("{} file(s) have other hard links: trashing removes one name, the data stays", linked.len()),
		);
	}

	let cross = plan.ops.iter().filter(|op| matches!(op, Op::Move { from, to } if crosses_devices(from, to))).count();
	if cross > 0 {
		raise(Level::Medium, format!("{cross} move(s) cross file systems: data is copied, undo copies it back"));
	}

	Risk { level, reasons }
}

/// Number of hard links of a regular file, when there is more than one.
#[cfg(unix)]
fn hard_links(p: &Path) -> Option<u64> {
	use std::os::unix::fs::MetadataExt;
	fs::symlink_metadata(p).ok().filter(|m| m.is_file() && m.nlink() > 1).map(|m| m.nlink())
}

#[cfg(not(unix))]
fn hard_links(_: &Path) -> Option<u64> {
	None
}

fn is_dir(p: &Path) -> bool {
	fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

#[cfg(unix)]
fn crosses_devices(from: &Path, to: &Path) -> bool {
	use std::os::unix::fs::MetadataExt;
	let dev = |p: &Path| fs::metadata(p).map(|m| m.dev()).ok();
	let target_dir = normalize(to).and_then(|t| t.parent().map(Path::to_path_buf));
	match (dev(from), target_dir.and_then(|d| dev(&d))) {
		(Some(a), Some(b)) => a != b,
		_ => false,
	}
}

/// Windows: a different drive or share (path prefix) means a different volume.
#[cfg(not(unix))]
fn crosses_devices(from: &Path, to: &Path) -> bool {
	use std::path::Component;
	let prefix = |p: &Path| {
		normalize(p).and_then(|p| match p.components().next() {
			Some(Component::Prefix(x)) => Some(x.as_os_str().to_ascii_lowercase()),
			_ => None,
		})
	};
	matches!((prefix(from), prefix(to)), (Some(a), Some(b)) if a != b)
}
