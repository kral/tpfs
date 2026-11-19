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
#[non_exhaustive]
pub enum Level {
	Low,
	Medium,
	High,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Risk {
	pub level: Level,
	pub reasons: Vec<String>,
}

pub fn assess(plan: &Plan, source: Source) -> Risk {
	// Judge the paths the executor will act on (`a/tmp/../Photos` is `a/Photos`).
	let normalized = plan.normalized();
	let plan = &normalized;
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

/// Number of hard links of a regular file, when there is more than one (NTFS has them too).
fn hard_links(p: &Path) -> Option<u64> {
	if !fs::symlink_metadata(p).is_ok_and(|m| m.is_file()) {
		return None;
	}
	crate::fsops::link_count(p).filter(|&n| n > 1)
}

fn is_dir(p: &Path) -> bool {
	fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

/// Whether moving `from` to `to` crosses file systems (becomes a copy plus a removal).
fn crosses_devices(from: &Path, to: &Path) -> bool {
	match normalize(to).and_then(|t| t.parent().map(Path::to_path_buf)) {
		Some(dir) => crate::path::crosses_devices(from, &dir),
		None => false,
	}
}

/// How many operations of one kind a plan has, and how much data they move, copy or trash.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Total {
	/// `move`, `copy`, `rename`, `trash`, `mkdir` or `rmdir`.
	pub kind: &'static str,
	pub count: usize,
	/// Bytes of the sources (files and whole folders), for move, copy and trash.
	pub bytes: u64,
	/// The size is a lower bound: a folder was too big to measure in full.
	pub more: bool,
}

/// Per-kind totals of a plan in a fixed order, with the bytes each kind touches. Folders are
/// measured without following links, up to a bounded number of entries per plan.
pub fn totals(plan: &Plan) -> Vec<Total> {
	const KINDS: [&str; 6] = ["move", "copy", "rename", "trash", "mkdir", "rmdir"];
	let mut out: Vec<Total> = KINDS.iter().map(|&kind| Total { kind, count: 0, bytes: 0, more: false }).collect();
	let mut budget = 200_000usize;
	// Measure the paths the executor will act on (`a/tmp/../x` is `a/x`).
	let plan = &plan.normalized();
	for op in &plan.ops {
		let (i, source) = match op {
			Op::Move { from, .. } => (0, Some(from)),
			Op::Copy { from, .. } => (1, Some(from)),
			Op::Rename { .. } => (2, None),
			Op::Trash { path } => (3, Some(path)),
			Op::Mkdir { .. } => (4, None),
			Op::Rmdir { .. } => (5, None),
		};
		out[i].count += 1;
		if let Some(p) = source {
			let (bytes, complete) = tree_bytes(p, &mut budget);
			out[i].bytes = out[i].bytes.saturating_add(bytes);
			out[i].more |= !complete;
		}
	}
	out.retain(|t| t.count > 0);
	out
}

fn tree_bytes(path: &Path, budget: &mut usize) -> (u64, bool) {
	let Ok(m) = fs::symlink_metadata(path) else { return (0, true) };
	if !m.is_dir() {
		return (m.len(), true);
	}
	let mut total = 0u64;
	let mut stack = vec![path.to_path_buf()];
	while let Some(dir) = stack.pop() {
		let Ok(rd) = fs::read_dir(&dir) else { continue };
		for e in rd.flatten() {
			if *budget == 0 {
				return (total, false);
			}
			*budget -= 1;
			match e.file_type() {
				Ok(t) if t.is_dir() => stack.push(e.path()),
				Ok(t) if t.is_file() => total = total.saturating_add(e.metadata().map_or(0, |m| m.len())),
				_ => {}
			}
		}
	}
	(total, true)
}

/// A size in the units people read: "512 B", "4.1 MB", "2.3 GB" (decimal, like file managers).
pub fn human_bytes(bytes: u64) -> String {
	const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
	if bytes < 1000 {
		return format!("{bytes} B");
	}
	let mut v = bytes as f64 / 1000.0;
	let mut unit = 0;
	// 999.95 would print as "1000.0": move up a unit once it rounds that far.
	while v >= 999.95 && unit < UNITS.len() - 1 {
		v /= 1000.0;
		unit += 1;
	}
	format!("{v:.1} {}", UNITS[unit])
}

/// Totals as one line: "2 copy (4.1 MB) · 1 trash (12 B)".
pub fn totals_line(plan: &Plan, separator: &str) -> String {
	totals(plan)
		.iter()
		.map(|t| match t.kind {
			"move" | "copy" | "trash" => {
				format!("{} {} ({}{})", t.count, t.kind, human_bytes(t.bytes), if t.more { "+" } else { "" })
			}
			_ => format!("{} {}", t.count, t.kind),
		})
		.collect::<Vec<_>>()
		.join(separator)
}

#[cfg(test)]
mod tests {
	use super::human_bytes;

	#[test]
	fn sizes_read_like_a_file_manager() {
		assert_eq!(human_bytes(0), "0 B");
		assert_eq!(human_bytes(999), "999 B");
		assert_eq!(human_bytes(1000), "1.0 KB");
		assert_eq!(human_bytes(999_950), "1.0 MB");
		assert_eq!(human_bytes(4_100_000), "4.1 MB");
		assert!(human_bytes(u64::MAX).ends_with("PB"));
	}
}
