//! What the trash holds, and emptying it: the one permanent delete in tpfs.
//!
//! Every batch keeps what it trashed under `<trash>/<batch>…/<step>/<name>`, in the trash of the
//! state folder or (Linux, macOS) in a `.tpfs-trash-<uid>` folder at the top of another disk. The
//! folders of one batch are `<batch>` and the ones undo and recovery made for it (`<batch>-undo`,
//! `<batch>-recover`, …). Emptying writes an [`Entry::Purge`] for each batch *before* deleting
//! anything, so a batch is never shown as undoable with half of its items gone; a run cut short is
//! finished by the next one.

use std::{
	collections::HashMap,
	fs, io,
	path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{
	executor::{ExecError, absolute_real, crash_point, in_local_trash, local_trash, now_secs},
	fsops::{Fs, remove_part, sync_parent},
	journal::{BatchStatus, Entry, Journal},
	op::Op,
};

/// What one batch left in the trash.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Trashed {
	pub id: String,
	/// When the batch ran (Unix seconds); for a batch the journal no longer knows, read from its id.
	pub at: u64,
	/// `human` or `agent`; empty when the journal no longer knows the batch.
	pub source: String,
	pub reason: String,
	/// The batch's state in the journal or its archive; `None` when neither knows it.
	pub status: Option<BatchStatus>,
	/// Known only from the archive (it can no longer be undone).
	pub archived: bool,
	pub damaged: bool,
	/// Its trash was already being emptied: a run cut short (the next one finishes it).
	pub purged: bool,
	/// Its folders in the trash folders, each `<trash>/<label>`.
	pub dirs: Vec<PathBuf>,
	/// Trashed items (one per step, as a rule).
	pub items: usize,
	/// Each item where it is in the trash, and where it came from when the journal says so.
	pub entries: Vec<(PathBuf, Option<PathBuf>)>,
	/// Their size in bytes (links counted by their own size, never followed).
	pub bytes: u64,
}

impl Trashed {
	/// `tpfs undo` can still put it back.
	pub fn undoable(&self) -> bool {
		self.status == Some(BatchStatus::Committed) && !self.archived && !self.damaged && !self.purged
	}

	/// An interrupted batch keeps its trash: `tpfs recover` needs it.
	pub fn emptiable(&self) -> bool {
		self.status != Some(BatchStatus::Incomplete)
	}

	/// Whether it can still be undone, in a few words.
	pub fn undo_state(&self) -> &'static str {
		match self.status {
			_ if self.purged => "no (emptying was cut short)",
			_ if self.archived => "no (archived)",
			Some(BatchStatus::Incomplete) => "no (interrupted: `tpfs recover`)",
			_ if self.damaged => "no (damaged)",
			Some(BatchStatus::Committed) => "yes",
			Some(BatchStatus::Undone) => "no (undone)",
			Some(BatchStatus::Aborted) => "no (rolled back)",
			Some(BatchStatus::Abandoned) => "no (abandoned)",
			_ => "no (not in the journal)",
		}
	}
}

/// Outcome of [`empty`].
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Emptied {
	pub batches: usize,
	pub items: usize,
	pub bytes: u64,
	/// Folders that could not be deleted; the next `tpfs trash empty` tries them again.
	pub errors: Vec<String>,
}

/// Everything the trash folders hold, per batch, oldest first: the trash of the state folder and
/// the per-disk trash folders the journal (or its archive) names that are there now.
pub fn contents(journal: &mut Journal, trash: &Path) -> io::Result<Vec<Trashed>> {
	let trash = absolute_real(trash);
	let live = journal.batches()?;
	let archived = journal.archived()?;
	let mut known: HashMap<&str, (&crate::journal::BatchView, bool)> = HashMap::new();
	for b in &archived {
		known.insert(b.id.as_str(), (b, true));
	}
	for b in &live {
		known.insert(b.id.as_str(), (b, false));
	}
	// Where each journaled trash step put its item, and where that item came from.
	let origin: HashMap<&Path, &Path> = live
		.iter()
		.chain(&archived)
		.flat_map(|b| &b.steps)
		.filter_map(|s| match (&s.forward, &s.inverse) {
			(Op::Trash { .. }, Op::Move { from, to }) => Some((from.as_path(), to.as_path())),
			_ => None,
		})
		.collect();
	let mut out: Vec<Trashed> = Vec::new();
	let mut at: HashMap<String, usize> = HashMap::new();
	for base in trash_folders(&trash, live.iter().chain(&archived).flat_map(|b| &b.steps)) {
		let names = match fs::read_dir(&base) {
			Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.file_name())).collect::<Vec<_>>(),
			Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
			Err(e) => return Err(e),
		};
		for name in names {
			let Some(id) = name.to_str().and_then(batch_of) else { continue };
			let dir = base.join(&name);
			let (entries, bytes) = measure(&dir);
			let i = *at.entry(id.to_owned()).or_insert_with(|| {
				let b = known.get(id);
				out.push(Trashed {
					id: id.to_owned(),
					at: b.map_or_else(|| time_of(id), |(b, _)| b.at),
					source: b.map(|(b, _)| b.source.clone()).unwrap_or_default(),
					reason: b.map(|(b, _)| b.reason.clone()).unwrap_or_default(),
					status: b.map(|(b, _)| b.status),
					archived: b.is_some_and(|(_, a)| *a),
					damaged: b.is_some_and(|(b, _)| b.damaged),
					purged: b.is_some_and(|(b, _)| b.purged),
					dirs: Vec::new(),
					items: 0,
					entries: Vec::new(),
					bytes: 0,
				});
				out.len() - 1
			});
			let t = &mut out[i];
			t.dirs.push(dir);
			t.items += entries.len();
			t.bytes += bytes;
			t.entries.extend(entries.into_iter().map(|e| {
				let from = origin.get(e.as_path()).map(|p| p.to_path_buf());
				(e, from)
			}));
		}
	}
	out.sort_by(|a, b| (a.at, &a.id).cmp(&(b.at, &b.id)));
	Ok(out)
}

/// What a person approves: the digest of exactly these batches' folders and what is in them.
/// `tpfs trash empty --approve <token>` deletes only a selection with the same token.
pub fn token(chosen: &[Trashed]) -> String {
	let mut h = Sha256::new();
	let mut chosen: Vec<&Trashed> = chosen.iter().collect();
	chosen.sort_by(|a, b| a.id.cmp(&b.id));
	for t in chosen {
		h.update(format!("{}\0{}\0{}\0", t.id, t.items, t.bytes));
		for p in t.dirs.iter().chain(t.entries.iter().map(|(p, _)| p)) {
			h.update(p.as_os_str().as_encoded_bytes());
			h.update([0]);
		}
	}
	h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Delete for good what the batches `ids` left in the trash, if what they hold now still has the
/// `expected` [`token`] (the selection the person approved). Interrupted emptying runs are
/// finished whatever is chosen, and are part of the token. Each batch the journal knows gets an
/// [`Entry::Purge`] before anything is deleted; deletion stays inside the trash folders, removes
/// links rather than what they point to, and makes read-only folders writable first.
pub fn empty(journal: &mut Journal, trash: &Path, ids: &[String], expected: &str) -> Result<Emptied, ExecError> {
	let trash = absolute_real(trash);
	let all = contents(journal, &trash)?;
	let chosen: Vec<Trashed> = all.into_iter().filter(|t| t.purged || ids.contains(&t.id)).collect();
	if token(&chosen) != expected {
		return Err(ExecError::Blocked(
			"the trash changed since it was shown; nothing was deleted. Run `tpfs trash empty` again".into(),
		));
	}
	if let Some(t) = chosen.iter().find(|t| !t.emptiable()) {
		return Err(ExecError::Blocked(format!(
			"batch {} was interrupted and `tpfs recover` needs its trash; nothing was deleted",
			crate::sanitize(t.id.as_ref())
		)));
	}
	let live: Vec<String> = journal.batches()?.into_iter().map(|b| b.id).collect();
	let at = now_secs();
	for t in chosen.iter().filter(|t| !t.purged && live.contains(&t.id)) {
		journal.write(&Entry::Purge { batch: t.id.clone(), at })?;
	}
	crash_point("purge-recorded")?;

	let bases: Vec<PathBuf> =
		chosen.iter().flat_map(|t| &t.dirs).filter_map(|d| d.parent()).map(Path::to_path_buf).collect();
	let local: Vec<PathBuf> = bases.iter().filter(|b| **b != trash).cloned().collect();
	let fs = Fs::confined_with_private(std::slice::from_ref(&trash), &local);
	let mut report = Emptied::default();
	for t in &chosen {
		let mut ok = true;
		for dir in &t.dirs {
			// Only a folder directly in a trash folder of ours: the state folder's, or a genuine
			// per-disk one (a private `.tpfs-trash-<uid>` owned by this user).
			let ours = dir.parent().is_some_and(|b| *b == trash || in_local_trash(b));
			let result = if ours {
				remove_part(&fs, dir)
			} else {
				Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused: not in a trash folder of tpfs"))
			};
			match result {
				Ok(()) => sync_parent(dir),
				Err(e) => {
					ok = false;
					report.errors.push(crate::sanitize(format!("{}: {e}", dir.display()).as_ref()));
				}
			}
		}
		if ok {
			report.batches += 1;
			report.items += t.items;
			report.bytes += t.bytes;
		}
	}
	Ok(report)
}

/// The trash folders to look in: the state folder's, and every per-disk one the journal names
/// (where a step trashed to, or where an undo could have trashed a copy) that is there now.
fn trash_folders<'a>(trash: &Path, steps: impl Iterator<Item = &'a crate::journal::Step>) -> Vec<PathBuf> {
	let mut out = vec![trash.to_path_buf()];
	let mut seen = std::collections::HashSet::new();
	let marker = ".tpfs-trash-";
	for step in steps {
		for (op, p) in
			[&step.forward, &step.inverse].into_iter().flat_map(|op| op.paths().into_iter().map(move |p| (op, p)))
		{
			let candidate = if p.as_os_str().to_string_lossy().contains(marker) {
				p.ancestors()
					.find(|a| a.file_name().is_some_and(|n| n.to_string_lossy().starts_with(marker)))
					.map(Path::to_path_buf)
					.filter(|a| in_local_trash(a))
			} else if matches!(op, Op::Copy { .. } | Op::Move { .. }) {
				// A copy's (or a moved item's) volume: undo may have trashed it there.
				let Some(dir) = p.parent() else { continue };
				if !seen.insert(dir.to_path_buf()) {
					continue;
				}
				dir.ancestors().find(|a| fs::symlink_metadata(a).is_ok()).and_then(|a| local_trash(trash, a, false))
			} else {
				None
			};
			if let Some(c) = candidate
				&& !out.contains(&c)
			{
				out.push(c);
			}
		}
	}
	out
}

/// The batch a trash folder belongs to: `<batch>` or `<batch>-<what>` (`-undo`, `-recover`, …),
/// with batch ids of the form `<hex>-<hex>`.
fn batch_of(name: &str) -> Option<&str> {
	let mut parts = name.splitn(3, '-');
	let (a, b) = (parts.next()?, parts.next()?);
	let hex = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_hexdigit());
	(hex(a) && hex(b)).then(|| &name[..a.len() + 1 + b.len()])
}

/// A batch id starts with the time it was made in nanoseconds (hex).
fn time_of(id: &str) -> u64 {
	id.split('-').next().and_then(|n| u128::from_str_radix(n, 16).ok()).map_or(0, |n| (n / 1_000_000_000) as u64)
}

/// Items (entries in the step folders) and bytes below one batch folder, without following links
/// (an explicit stack: no recursion however deep the tree).
fn measure(dir: &Path) -> (Vec<PathBuf>, u64) {
	let (mut items, mut bytes) = (Vec::new(), 0);
	let mut stack = vec![(dir.to_path_buf(), 0usize)];
	while let Some((path, depth)) = stack.pop() {
		let Ok(m) = fs::symlink_metadata(&path) else { continue };
		if depth == 2 {
			items.push(path.clone());
		}
		if !m.is_dir() {
			bytes += m.len();
			continue;
		}
		if let Ok(rd) = fs::read_dir(&path) {
			stack.extend(rd.filter_map(|e| e.ok()).map(|e| (e.path(), depth + 1)));
		}
	}
	items.sort();
	(items, bytes)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Executor, Plan, Source, Validator, executor::CRASH_AT};

	/// A crash between the purge records and the deletion: the batch already counts as emptied
	/// (not undoable), and the next run deletes the rest whatever it was asked to choose.
	#[test]
	fn a_crash_after_the_purge_record_is_finished_by_the_next_run() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(tmp.path()).unwrap();
		let (root, state) = (base.join("root"), base.join("state"));
		fs::create_dir(&root).unwrap();
		fs::write(root.join("a.txt"), "A").unwrap();
		let v = Validator::new([&root]).unwrap();
		let mut j = Journal::open(state.join("journal.jsonl")).unwrap();
		let trash = state.join("trash");
		let plan = Plan::new(vec![Op::Trash { path: root.join("a.txt") }], "t");
		let id = Executor::new(&v, &mut j, &trash).execute(&plan, Source::Human).unwrap().id;

		let all = contents(&mut j, &trash).unwrap();
		assert_eq!((all.len(), all[0].items, all[0].bytes), (1, 1, 1));
		let tok = token(&all);
		CRASH_AT.with(|c| c.set(Some("purge-recorded")));
		let err = empty(&mut j, &trash, std::slice::from_ref(&id), &tok).unwrap_err();
		CRASH_AT.with(|c| c.set(None));
		assert!(err.to_string().contains("simulated crash"), "{err}");

		// Recorded, nothing deleted yet: no longer undoable, and undo says why.
		let left = contents(&mut j, &trash).unwrap();
		assert!(left[0].purged && !left[0].undoable() && trash.join(&id).exists());
		let err = Executor::new(&v, &mut j, &trash).undo_last().unwrap_err();
		assert!(err.to_string().contains("trash was emptied"), "{err}");

		// The next run, with nothing chosen, finishes it.
		let done = empty(&mut j, &trash, &[], &token(&left)).unwrap();
		assert_eq!((done.batches, done.errors.len()), (1, 0));
		assert!(!trash.join(&id).exists());
		assert!(contents(&mut j, &trash).unwrap().is_empty());
	}

	#[test]
	fn batch_folders_are_told_by_their_id() {
		assert_eq!(batch_of("18e97ddd3e09330c-6c01"), Some("18e97ddd3e09330c-6c01"));
		assert_eq!(batch_of("18e97ddd3e09330c-6c01-undo-1"), Some("18e97ddd3e09330c-6c01"));
		assert_eq!(batch_of("notes"), None);
		assert_eq!(batch_of(".tpfs-part-x"), None);
		assert_eq!(time_of("3b9aca00-1"), 1);
	}
}
