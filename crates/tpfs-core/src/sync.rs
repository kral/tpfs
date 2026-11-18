//! Compare two folders and turn the differences into an ordinary [`Plan`].
//!
//! Synchronisation adds no new power: it only *proposes* `copy`, `mkdir` and `trash` operations,
//! which go through the same validation, preview, journal and undo as everything else. A newer
//! file replaces an older one by trashing the old one first, so the old version stays restorable.

use std::{
	collections::BTreeMap,
	fs, io,
	path::{Path, PathBuf},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{
	display::sanitize,
	op::{Op, Plan},
};

/// Modification times closer than this count as equal: FAT and many NAS file systems store
/// times with 2-second resolution, and copies across file systems may round.
pub const MTIME_TOLERANCE: Duration = Duration::from_secs(2);

/// Stop walking after this many entries, so a mistaken root cannot hang the caller.
pub const MAX_ENTRIES: usize = 200_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
	File,
	Dir,
	/// Symlinks and special files are reported but never synchronised.
	Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Diff {
	OnlyLeft,
	OnlyRight,
	LeftNewer,
	RightNewer,
	/// Same time (within the tolerance) but a different size: nobody can tell which one is right.
	Conflict,
	/// A file on one side, a folder (or something else) on the other.
	TypeMismatch,
	Same,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
	pub kind: Kind,
	pub size: u64,
	/// Milliseconds since the Unix epoch.
	pub modified: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
	/// Path relative to both roots.
	pub rel: PathBuf,
	pub diff: Diff,
	pub left: Option<Meta>,
	pub right: Option<Meta>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comparison {
	/// Every difference, in path order. Identical files are counted, not listed.
	pub items: Vec<Item>,
	pub same: usize,
	/// Set when the walk stopped at [`MAX_ENTRIES`]; the result is then incomplete.
	pub truncated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
	#[default]
	LeftToRight,
	RightToLeft,
	/// Copy what is missing on either side and let the newer file win.
	Both,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncOptions {
	pub direction: Direction,
	/// One-way only: move what exists only on the target side to the trash, so the target becomes
	/// a copy of the source. Undoable like everything else, but rated as high risk.
	#[serde(default)]
	pub mirror: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skipped {
	pub rel: PathBuf,
	pub why: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncPlan {
	pub plan: Plan,
	/// Differences the plan deliberately leaves alone, with the reason.
	pub skipped: Vec<Skipped>,
}

/// Compare `left` and `right`. With `recursive`, folders present on both sides are compared
/// entry by entry; a folder present on one side only is one item (copying it copies everything).
pub fn compare(left: &Path, right: &Path, recursive: bool) -> io::Result<Comparison> {
	let mut out = Comparison::default();
	let mut budget = MAX_ENTRIES;
	walk(left, right, Path::new(""), recursive, &mut out, &mut budget)?;
	Ok(out)
}

fn walk(
	left: &Path,
	right: &Path,
	rel: &Path,
	recursive: bool,
	out: &mut Comparison,
	budget: &mut usize,
) -> io::Result<()> {
	let l = list(&left.join(rel), budget)?;
	let r = list(&right.join(rel), budget)?;
	if *budget == 0 {
		out.truncated = true;
	}

	let mut names: Vec<&std::ffi::OsString> = l.keys().chain(r.keys()).collect();
	names.sort();
	names.dedup();

	for name in names {
		let rel = rel.join(name);
		let (lm, rm) = (l.get(name).cloned(), r.get(name).cloned());
		let diff = match (&lm, &rm) {
			(Some(_), None) => Diff::OnlyLeft,
			(None, Some(_)) => Diff::OnlyRight,
			(Some(a), Some(b)) if a.kind != b.kind || a.kind == Kind::Other => Diff::TypeMismatch,
			(Some(a), Some(_)) if a.kind == Kind::Dir => {
				if recursive && !out.truncated {
					walk(left, right, &rel, true, out, budget)?;
				}
				continue;
			}
			(Some(a), Some(b)) => file_diff(a, b),
			(None, None) => unreachable!("name comes from one of the listings"),
		};
		if diff == Diff::Same {
			out.same += 1;
		} else {
			out.items.push(Item { rel, diff, left: lm, right: rm });
		}
	}
	Ok(())
}

fn file_diff(a: &Meta, b: &Meta) -> Diff {
	let tol = MTIME_TOLERANCE.as_millis() as i64;
	match (a.modified, b.modified) {
		(Some(x), Some(y)) if (x - y).abs() <= tol => {
			if a.size == b.size {
				Diff::Same
			} else {
				Diff::Conflict
			}
		}
		(Some(x), Some(y)) => {
			if x > y {
				Diff::LeftNewer
			} else {
				Diff::RightNewer
			}
		}
		_ if a.size == b.size => Diff::Same,
		_ => Diff::Conflict,
	}
}

/// Directory listing without following symlinks. A missing directory is an empty one.
fn list(dir: &Path, budget: &mut usize) -> io::Result<BTreeMap<std::ffi::OsString, Meta>> {
	let mut out = BTreeMap::new();
	let rd = match fs::read_dir(dir) {
		Ok(rd) => rd,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
		Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", sanitize(dir.as_os_str())))),
	};
	for entry in rd {
		if *budget == 0 {
			break;
		}
		*budget -= 1;
		let entry = entry?;
		let Ok(m) = entry.metadata() else { continue };
		let ft = m.file_type();
		let kind = if ft.is_dir() {
			Kind::Dir
		} else if ft.is_file() {
			Kind::File
		} else {
			Kind::Other
		};
		let modified = m.modified().ok().and_then(millis);
		out.insert(entry.file_name(), Meta { kind, size: if kind == Kind::File { m.len() } else { 0 }, modified });
	}
	Ok(out)
}

/// What a sync does with one difference. The single source of truth for both the plan and any
/// preview of it (the commander's sync dialog shows these before the plan is built).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Action {
	CopyToRight,
	CopyToLeft,
	/// Trash the right one, then copy the left one in its place.
	ReplaceRight,
	ReplaceLeft,
	/// Mirror: trash an item that exists only on the target side.
	TrashLeft,
	TrashRight,
	Skip(&'static str),
}

impl Action {
	pub fn label(self) -> &'static str {
		match self {
			Action::CopyToRight => "copy →",
			Action::CopyToLeft => "← copy",
			Action::ReplaceRight => "replace →",
			Action::ReplaceLeft => "← replace",
			Action::TrashLeft => "trash left",
			Action::TrashRight => "trash right",
			Action::Skip(_) => "skip",
		}
	}
}

/// The action for one item under the given options.
pub fn action(item: &Item, opts: SyncOptions) -> Action {
	use Action::*;
	use Diff::*;
	use Direction::*;

	let mirror = opts.mirror && opts.direction != Both;
	let other = |m: &Option<Meta>| m.as_ref().is_some_and(|m| m.kind == Kind::Other);
	match (item.diff, opts.direction) {
		(OnlyLeft | OnlyRight, _) if other(&item.left) || other(&item.right) => Skip("symlink or special file"),
		(OnlyLeft, LeftToRight | Both) => CopyToRight,
		(OnlyRight, RightToLeft | Both) => CopyToLeft,
		(OnlyLeft, RightToLeft) if mirror => TrashLeft,
		(OnlyRight, LeftToRight) if mirror => TrashRight,
		(OnlyLeft, RightToLeft) => Skip("only on the left; mirror is off"),
		(OnlyRight, LeftToRight) => Skip("only on the right; mirror is off"),
		(LeftNewer, LeftToRight | Both) => ReplaceRight,
		(RightNewer, RightToLeft | Both) => ReplaceLeft,
		(LeftNewer, RightToLeft) => Skip("the left one is newer; would replace a newer file"),
		(RightNewer, LeftToRight) => Skip("the right one is newer; would replace a newer file"),
		(Conflict, _) => Skip("same time, different size; decide by hand"),
		(TypeMismatch, _) => Skip("file on one side, folder or link on the other"),
		(Same, _) => Skip("identical"),
	}
}

/// Turn a comparison into a plan. Never overwrites: a newer file replaces an older one by trashing
/// the old one first. Conflicts, type mismatches, symlinks and changes against the chosen direction
/// are skipped and reported.
pub fn sync_plan(left: &Path, right: &Path, cmp: &Comparison, opts: SyncOptions) -> SyncPlan {
	let mut ops = Vec::new();
	let mut skipped = Vec::new();
	let (mut copies, mut replaced, mut trashed) = (0usize, 0usize, 0usize);
	let (l_name, r_name) = (sanitize(left.as_os_str()), sanitize(right.as_os_str()));

	for item in cmp.items.iter().filter(|i| i.diff != Diff::Same) {
		let (l, r) = (left.join(&item.rel), right.join(&item.rel));
		match action(item, opts) {
			Action::CopyToRight => {
				ops.push(Op::Copy { from: l, to: r });
				copies += 1;
			}
			Action::CopyToLeft => {
				ops.push(Op::Copy { from: r, to: l });
				copies += 1;
			}
			Action::ReplaceRight => {
				ops.push(Op::Trash { path: r.clone() });
				ops.push(Op::Copy { from: l, to: r });
				replaced += 1;
			}
			Action::ReplaceLeft => {
				ops.push(Op::Trash { path: l.clone() });
				ops.push(Op::Copy { from: r, to: l });
				replaced += 1;
			}
			Action::TrashLeft => {
				ops.push(Op::Trash { path: l });
				trashed += 1;
			}
			Action::TrashRight => {
				ops.push(Op::Trash { path: r });
				trashed += 1;
			}
			Action::Skip(why) => skipped.push(Skipped { rel: item.rel.clone(), why: why.to_owned() }),
		}
	}

	let arrow = match opts.direction {
		Direction::LeftToRight => format!("{l_name} → {r_name}"),
		Direction::RightToLeft => format!("{r_name} → {l_name}"),
		Direction::Both => format!("{l_name} ↔ {r_name}"),
	};
	let mut parts = vec![format!("{copies} to copy")];
	if replaced > 0 {
		parts.push(format!("{replaced} to replace (old versions to trash)"));
	}
	if trashed > 0 {
		parts.push(format!("{trashed} to trash (mirror)"));
	}
	let reason = format!("Sync {arrow}: {}", parts.join(", "));
	SyncPlan { plan: Plan::new(ops, reason), skipped }
}

/// For callers that need a timestamp in the same form as [`Meta::modified`].
pub fn millis(t: SystemTime) -> Option<i64> {
	t.duration_since(UNIX_EPOCH).ok().map(|d| d.as_millis() as i64)
}
