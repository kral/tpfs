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
#[non_exhaustive]
pub enum Diff {
	OnlyLeft,
	OnlyRight,
	LeftNewer,
	RightNewer,
	/// Same time (within the tolerance) but a different size: nobody can tell which one is right.
	Conflict,
	/// A file on one side, a folder (or something else) on the other.
	TypeMismatch,
	/// The same name in a different letter case on a file system that ignores case.
	CaseMismatch,
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
/// The two folders must be separate: if one contains the other, a sync would copy into itself.
pub fn compare(left: &Path, right: &Path, recursive: bool) -> io::Result<Comparison> {
	if left.starts_with(right) || right.starts_with(left) {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			"the two folders must be separate (one is inside the other)",
		));
	}
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
	let (l, lt) = list(&left.join(rel), budget)?;
	let (r, rt) = list(&right.join(rel), budget)?;
	if lt || rt {
		out.truncated = true;
	}

	// On file systems that ignore letter case, `A.txt` and `a.txt` are one file: pairing them by
	// exact name would plan a copy onto an existing file. They are reported, not synced.
	if CASE_INSENSITIVE {
		let folded: BTreeMap<String, &std::ffi::OsString> =
			r.keys().map(|k| (k.to_string_lossy().to_lowercase(), k)).collect();
		for name in l.keys().filter(|k| !r.contains_key(*k)) {
			if folded.contains_key(&name.to_string_lossy().to_lowercase()) {
				out.items.push(Item {
					rel: rel.join(name),
					diff: Diff::CaseMismatch,
					left: l.get(name).cloned(),
					right: None,
				});
			}
		}
	}
	let case_twin = |name: &std::ffi::OsString, other: &BTreeMap<std::ffi::OsString, Meta>| {
		CASE_INSENSITIVE
			&& !other.contains_key(name)
			&& other.keys().any(|k| k.to_string_lossy().to_lowercase() == name.to_string_lossy().to_lowercase())
	};

	let mut names: Vec<&std::ffi::OsString> = l.keys().chain(r.keys()).collect();
	names.sort();
	names.dedup();

	for name in names {
		if case_twin(name, if l.contains_key(name) { &r } else { &l }) {
			continue; // reported above
		}
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

/// Windows and macOS (by default) ignore letter case in file names.
const CASE_INSENSITIVE: bool = cfg!(any(windows, target_vendor = "apple"));

fn file_diff(a: &Meta, b: &Meta) -> Diff {
	let tol = MTIME_TOLERANCE.as_millis() as u64;
	match (a.modified, b.modified) {
		(Some(x), Some(y)) if x.abs_diff(y) <= tol => {
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

/// Directory listing without following symlinks. A missing directory is an empty one. The flag
/// says the budget ran out while entries were left. Metadata comes from a fresh `lstat` per entry
/// (directory-listing data can be stale on Windows); an entry that cannot be read is listed as
/// `Other`, so it is skipped instead of looking absent (which `--mirror` would trash on the other side).
fn list(dir: &Path, budget: &mut usize) -> io::Result<(BTreeMap<std::ffi::OsString, Meta>, bool)> {
	let mut out = BTreeMap::new();
	let rd = match fs::read_dir(dir) {
		Ok(rd) => rd,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((out, false)),
		Err(e) => return Err(io::Error::new(e.kind(), format!("{}: {e}", sanitize(dir.as_os_str())))),
	};
	for entry in rd {
		let entry = entry?;
		if *budget == 0 {
			return Ok((out, true));
		}
		*budget -= 1;
		let meta = match fs::symlink_metadata(entry.path()) {
			Ok(m) => {
				let ft = m.file_type();
				let kind = if ft.is_dir() {
					Kind::Dir
				} else if ft.is_file() {
					Kind::File
				} else {
					Kind::Other
				};
				let modified = m.modified().ok().and_then(millis);
				Meta { kind, size: if kind == Kind::File { m.len() } else { 0 }, modified }
			}
			Err(_) => Meta { kind: Kind::Other, size: 0, modified: None },
		};
		out.insert(entry.file_name(), meta);
	}
	Ok((out, false))
}

/// What a sync does with one difference. The single source of truth for both the plan and any
/// preview of it (the commander's sync dialog shows these before the plan is built).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
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
		(CaseMismatch, _) => Skip("same name in a different letter case; rename one by hand"),
		(Same, _) => Skip("identical"),
	}
}

/// Turn a comparison into a plan. Never overwrites: a newer file replaces an older one by trashing
/// the old one first. Conflicts, type mismatches, symlinks and changes against the chosen direction
/// are skipped and reported.
///
/// An incomplete comparison (`truncated`) gives an empty plan: entries that were not listed would
/// look missing, and syncing on that basis could copy or trash the wrong things.
pub fn sync_plan(left: &Path, right: &Path, cmp: &Comparison, opts: SyncOptions) -> SyncPlan {
	if cmp.truncated {
		return SyncPlan {
			plan: Plan::new(vec![], "Sync not planned: the comparison is incomplete (too many entries)"),
			skipped: vec![Skipped { rel: PathBuf::new(), why: "comparison incomplete; nothing planned".into() }],
		};
	}
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
	t.duration_since(UNIX_EPOCH).ok().and_then(|d| i64::try_from(d.as_millis()).ok())
}
