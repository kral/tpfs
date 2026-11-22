use std::{
	fs::{File, OpenOptions, TryLockError},
	io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
	path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::op::Op;

/// One record of the write-ahead journal.
///
/// A batch is written as `begin`, then for every step `intent` (with the inverse operation, fsynced
/// *before* the disk is touched) and `done` (after). It ends with `commit`, or `abort` when it was
/// rolled back. Undo writes `undo_step` per reverted step and `undo` when the whole batch is reverted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Entry {
	Begin {
		batch: String,
		at: u64,
		source: String,
		reason: String,
		digest: String,
		/// Record format version; a batch written by a newer tpfs is not replayed by this one.
		#[serde(default = "format_v1", skip_serializing_if = "is_v1")]
		v: u32,
	},
	Intent {
		batch: String,
		index: usize,
		forward: Op,
		inverse: Op,
	},
	Done {
		batch: String,
		index: usize,
		/// For copies: a fingerprint of the result, so undo can tell whether it was changed since.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		check: Option<String>,
		/// For `rmdir`, in journals of older versions: the folder's permission bits (now only in
		/// [`Entry::Mode`]). Still read.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		mode: Option<u32>,
	},
	/// A copy's fingerprint recorded separately (reserved; read for forward compatibility).
	Check {
		batch: String,
		index: usize,
		check: String,
	},
	/// For `rmdir`: the folder's permission bits, recorded before it is removed, so undo and
	/// recovery after a crash in between restore them.
	Mode {
		batch: String,
		index: usize,
		mode: u32,
	},
	/// Undo or recovery is about to rename the copy of a step's cross-device inverse move into place.
	UndoPublished {
		batch: String,
		index: usize,
	},
	/// A copy built under a private name is about to be renamed into place (written right before the
	/// rename, between `intent` and `done`). With the part name gone afterwards, the target is ours.
	Published {
		batch: String,
		index: usize,
	},
	/// A step whose own call failed: it changed nothing beyond tpfs's own marks, which were cleaned
	/// up. Rollback and recovery leave it alone instead of judging it from the disk.
	Failed {
		batch: String,
		index: usize,
	},
	Commit {
		batch: String,
	},
	Abort {
		batch: String,
	},
	/// An interrupted batch the user gave up on: its changes were left on disk as they were.
	Abandon {
		batch: String,
	},
	UndoStep {
		batch: String,
		index: usize,
	},
	Undo {
		batch: String,
	},
	/// `tpfs trash empty` is about to delete what the batch left in the trash, for good (written
	/// before anything is deleted). From here on the batch cannot be undone; a run cut short is
	/// finished by the next one.
	Purge {
		batch: String,
		#[serde(default)]
		at: u64,
	},
}

impl Entry {
	fn batch(&self) -> &str {
		match self {
			Entry::Begin { batch, .. }
			| Entry::Intent { batch, .. }
			| Entry::Done { batch, .. }
			| Entry::Published { batch, .. }
			| Entry::Check { batch, .. }
			| Entry::UndoPublished { batch, .. }
			| Entry::Failed { batch, .. }
			| Entry::Mode { batch, .. }
			| Entry::Commit { batch }
			| Entry::Abort { batch }
			| Entry::Abandon { batch }
			| Entry::UndoStep { batch, .. }
			| Entry::Purge { batch, .. }
			| Entry::Undo { batch } => batch,
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BatchStatus {
	/// Started but neither committed nor aborted — interrupted by a crash; `recover` rolls it back.
	Incomplete,
	Committed,
	Aborted,
	Undone,
	/// Given up on by the user (`tpfs recover --abandon`): not rolled back, changes left as they were.
	Abandoned,
}

/// A batch reconstructed from the journal.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BatchView {
	pub id: String,
	pub at: u64,
	pub source: String,
	pub reason: String,
	pub status: BatchStatus,
	/// An unreadable journal line sits inside this batch (later records of it follow the damage), or
	/// it was written by a newer format: its steps are not known for sure, so undo and recovery
	/// refuse it instead of guessing.
	pub damaged: bool,
	/// What it left in the trash was (or is being) deleted by `tpfs trash empty`: it can no
	/// longer be undone (see [`Entry::Purge`]).
	pub purged: bool,
	pub steps: Vec<Step>,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Step {
	pub index: usize,
	pub forward: Op,
	pub inverse: Op,
	pub done: bool,
	/// The step's copy reached its target (see [`Entry::Published`]).
	pub published: bool,
	/// See [`Entry::Done`].
	pub check: Option<String>,
	/// See [`Entry::Done`].
	pub mode: Option<u32>,
	/// See [`Entry::UndoPublished`].
	pub undo_published: bool,
	/// See [`Entry::Failed`].
	pub failed: bool,
	pub undone: bool,
}

/// Append-only journal, one CRC-protected JSON record per line, fsynced after every write.
/// Holds an exclusive lock: one engine writes, everyone else talks to that engine.
///
/// Finished history does not stay in it forever: once the journal is big, batches that can no
/// longer be acted on move to an archive next to it (see [`Journal::compact`]).
pub struct Journal {
	path: PathBuf,
	file: File,
	/// `journal.lock`, locked for as long as this journal is open: the journal file itself is
	/// replaced when it is compacted, so its own lock would not keep a second engine out.
	_lock: File,
	corrupt: usize,
}

/// The journal is compacted when it is opened at this size or more.
const COMPACT_AT: u64 = 4 << 20;
/// Committed batches kept undoable in the journal; older ones move to the archive.
pub const KEEP_COMMITTED: usize = 1000;

impl Journal {
	pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
		let path = path.as_ref().to_path_buf();
		let path = std::path::absolute(&path)?;
		if let Some(dir) = path.parent() {
			crate::path::private_dir(dir)?;
		}
		// The journal's own spelling, without symlinks on the way.
		let path = match (path.parent().map(crate::path::canonicalize), path.file_name()) {
			(Some(Ok(dir)), Some(name)) => dir.join(name),
			_ => path,
		};
		crate::path::check_private_file(&path)?;
		let lock = lock_file(&path.with_extension("lock"))?;

		let mut options = OpenOptions::new();
		options.create(true).append(true).read(true);
		#[cfg(unix)]
		std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
		let created = !path.exists();
		let file = options.open(&path)?;
		if created {
			// The journal's own directory entry must survive a power loss too.
			crate::fsops::sync_parent(&path);
		}

		match file.try_lock() {
			Ok(()) => {}
			Err(TryLockError::WouldBlock) => {
				return Err(io::Error::new(
					io::ErrorKind::WouldBlock,
					format!("journal is in use by another tpfs process: {}", path.display()),
				));
			}
			Err(TryLockError::Error(e)) => return Err(e),
		}

		let mut journal = Self { path, file, _lock: lock, corrupt: 0 };
		journal.seal_torn_tail()?;
		if journal.file.metadata()?.len() >= COMPACT_AT {
			// Best effort: the journal stays as it was if this fails part way.
			let _ = journal.compact();
		}
		Ok(journal)
	}

	/// Where compacted history goes: `journal-archive.jsonl` next to the journal.
	pub fn archive_path(&self) -> PathBuf {
		let stem = self.path.file_stem().map_or_else(|| "journal".into(), |s| s.to_string_lossy().into_owned());
		self.path.with_file_name(format!("{stem}-archive.jsonl"))
	}

	/// Move the batches nothing can act on any more into the archive, so the journal (read in full
	/// by every undo, recovery and plan) stays small. Archived are the batches that are undone,
	/// rolled back or abandoned, and committed ones older than the newest [`KEEP_COMMITTED`] —
	/// and only those that started before any interrupted or damaged batch, whose recovery looks at
	/// what came after it. Nothing is archived while the journal has unreadable lines (which batch
	/// they belong to decides what is damaged). Returns the number of batches archived.
	///
	/// Crash-safe: the new journal is written in full under a private name and synced, the archived
	/// records are appended to the archive and synced, and only then does the new journal replace
	/// the old one by a rename. A crash before the rename leaves the old journal (the archive may
	/// then hold some records twice, which only shows them twice).
	pub fn compact(&mut self) -> io::Result<usize> {
		let records = self.raw_records()?;
		if self.corrupt > 0 {
			return Ok(0);
		}
		let batches = fold(records.iter().map(|(_, e)| e.clone()));
		let first_open = batches.iter().position(|b| b.status == BatchStatus::Incomplete || b.damaged);
		let committed: Vec<usize> =
			batches.iter().enumerate().filter(|(_, b)| b.status == BatchStatus::Committed).map(|(i, _)| i).collect();
		let keep_from = committed.len().saturating_sub(KEEP_COMMITTED);
		let old_committed: std::collections::HashSet<usize> = committed[..keep_from].iter().copied().collect();
		let archived: std::collections::HashSet<&str> = batches
			.iter()
			.enumerate()
			.take(first_open.unwrap_or(batches.len()))
			.filter(|(i, b)| {
				matches!(b.status, BatchStatus::Undone | BatchStatus::Aborted | BatchStatus::Abandoned)
					|| old_committed.contains(i)
			})
			.map(|(_, b)| b.id.as_str())
			.collect();
		if archived.is_empty() {
			return Ok(0);
		}
		let (mut out, mut gone) = (Vec::new(), Vec::new());
		for (line, e) in &records {
			let to = if e.as_ref().is_some_and(|e| archived.contains(e.batch())) { &mut gone } else { &mut out };
			to.extend_from_slice(line.as_bytes());
			to.push(b'\n');
		}

		let tmp = self.path.with_extension("jsonl.compact");
		let new = private_file(&tmp, true)?;
		(&new).write_all(&out)?;
		new.sync_all()?;
		let archive_path = self.archive_path();
		crate::path::check_private_file(&archive_path)?;
		let archive = private_file(&archive_path, false)?;
		(&archive).write_all(&gone)?;
		archive.sync_all()?;
		crate::fsops::sync_parent(&archive_path);
		self.replace_with(new, &tmp)?;
		Ok(archived.len())
	}

	/// Put the compacted journal `new` (written at `tmp`) in place of the open one.
	fn replace_with(&mut self, new: File, tmp: &Path) -> io::Result<()> {
		// Windows cannot rename over a file that is still open; elsewhere the old one stays open
		// (and the journal readable) until the rename succeeded.
		#[cfg(windows)]
		{
			let old = std::mem::replace(&mut self.file, new);
			drop(old);
			if let Err(e) = std::fs::rename(tmp, &self.path) {
				self.file = OpenOptions::new().append(true).read(true).open(&self.path)?;
				return Err(e);
			}
		}
		#[cfg(not(windows))]
		{
			std::fs::rename(tmp, &self.path)?;
			self.file = new;
		}
		crate::fsops::sync_parent(&self.path);
		// Older tpfs versions lock the journal file itself: keep them out of the new one too.
		let _ = self.file.try_lock();
		Ok(())
	}

	/// The batches moved to the archive, oldest first (for `tpfs log --all`). Read only.
	pub fn archived(&self) -> io::Result<Vec<BatchView>> {
		let file = match File::open(self.archive_path()) {
			Ok(f) => f,
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
			Err(e) => return Err(e),
		};
		let mut corrupt = 0;
		let records = read_records(BufReader::new(file), &mut corrupt)?;
		// A record appended twice by a compaction cut short counts once.
		let mut seen = std::collections::HashSet::new();
		Ok(fold(records.into_iter().filter(|(line, _)| seen.insert(line.clone())).map(|(_, e)| e)))
	}

	/// A crash in the middle of an append leaves a last line without its newline. Terminate it, so
	/// the next record starts on a line of its own instead of being glued to the fragment (which
	/// would make both fail the CRC and silently drop the new record).
	fn seal_torn_tail(&mut self) -> io::Result<()> {
		let len = self.file.metadata()?.len();
		if len == 0 {
			return Ok(());
		}
		let mut last = [0u8; 1];
		let mut file = &self.file;
		file.seek(SeekFrom::Start(len - 1))?;
		file.read_exact(&mut last)?;
		if last[0] != b'\n' {
			self.file.write_all(b"\n")?;
			self.file.sync_data()?;
		}
		Ok(())
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Number of unreadable lines seen by the last read (e.g. a torn write after a crash).
	pub fn corrupt_lines(&self) -> usize {
		self.corrupt
	}

	/// Append a record by hand — for tests (feature `test-support`); the executor writes the journal.
	#[cfg(feature = "test-support")]
	pub fn append(&mut self, entry: &Entry) -> io::Result<()> {
		self.write(entry)
	}

	/// Append one record and fsync it. Undo and recovery never trust a record to lead outside the
	/// roots (see `Executor`).
	pub(crate) fn write(&mut self, entry: &Entry) -> io::Result<()> {
		let json = serde_json::to_string(entry).map_err(io::Error::other)?;
		let line = format!("{:08x} {json}\n", crc32fast::hash(json.as_bytes()));
		self.file.write_all(line.as_bytes())?;
		self.file.sync_data()
	}

	pub fn entries(&mut self) -> io::Result<Vec<Entry>> {
		Ok(self.records()?.into_iter().flatten().collect())
	}

	/// Every line in order: `None` for an unreadable one.
	fn records(&mut self) -> io::Result<Vec<Option<Entry>>> {
		Ok(self.raw_records()?.into_iter().map(|(_, e)| e).collect())
	}

	/// Every line in order with its text.
	fn raw_records(&mut self) -> io::Result<Vec<(String, Option<Entry>)>> {
		// Read through the locked handle itself: on Windows the lock is mandatory, so a second
		// handle could not read the file. Appends still go to the end regardless of the position.
		let mut file = &self.file;
		file.seek(SeekFrom::Start(0))?;
		self.corrupt = 0;
		read_records(BufReader::new(file), &mut self.corrupt)
	}

	/// All batches in journal order.
	pub fn batches(&mut self) -> io::Result<Vec<BatchView>> {
		Ok(fold(self.records()?))
	}
}

/// Rebuild the batches from the records in order (`None`: an unreadable line).
fn fold(records: impl IntoIterator<Item = Option<Entry>>) -> Vec<BatchView> {
	let mut batches: Vec<BatchView> = Vec::new();
	// Batch id -> position of its latest view: one lookup per record, not a scan of history.
	let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
	// Batches that were open when an unreadable line appeared, until the next readable record:
	// (id, whether it was committed — then an undo of it may have lost a record).
	let mut suspect: Vec<(String, bool)> = Vec::new();
	// The batch of the last readable record: the one being written when damage appears (an
	// undo may be writing an older batch than the newest one).
	let mut writing: Option<String> = None;
	// An unreadable line was just seen: the next readable record tells which batch was being
	// written too (an undo of an older batch may have lost its first record to it).
	let mut after_damage = false;
	for e in records {
		let Some(e) = e else {
			after_damage = true;
			// Only the batch being written when the damage happened can have lost a record to it;
			// an older batch is not affected by later damage.
			let current = writing.as_ref().and_then(|id| index.get(id)).and_then(|&i| batches.get(i));
			if let Some(b) = current.or(batches.last())
				&& matches!(b.status, BatchStatus::Incomplete | BatchStatus::Committed)
				&& !suspect.iter().any(|(id, _)| *id == b.id)
			{
				suspect.push((b.id.clone(), b.status == BatchStatus::Committed));
			}
			continue;
		};
		if !suspect.is_empty() {
			let ours = |id: &str| suspect.iter().find(|(s, _)| s == id);
			// Recovery's own records for an interrupted batch (it may need several runs) are the
			// normal end of a crash with a torn last line; anything else after the damage means
			// records of the open batch may be lost (an intent, a done, the commit itself).
			let recovery = match &e {
				Entry::Abort { batch } | Entry::UndoPublished { batch, .. } | Entry::UndoStep { batch, .. } => {
					ours(batch).is_some_and(|(_, committed)| !committed)
				}
				_ => false,
			};
			if !recovery {
				for (id, committed) in &suspect {
					let Some(b) = index.get(id).and_then(|&i| batches.get_mut(i)) else { continue };
					// A committed batch lost a record only if it was being undone: its own record
					// follows, or an undo of it had already started.
					let undoing = e.batch() == id || b.steps.iter().any(|s| s.undone || s.undo_published);
					if !committed || undoing {
						b.damaged = true;
					}
				}
			}
			// After recovery's intermediate records the batch is still open: keep watching it.
			if !matches!(&e, Entry::UndoPublished { .. } | Entry::UndoStep { .. }) || !recovery {
				suspect.clear();
			}
		}
		if std::mem::take(&mut after_damage)
			&& matches!(e, Entry::UndoStep { .. } | Entry::UndoPublished { .. } | Entry::Undo { .. })
			&& let Some(b) = index.get(e.batch()).and_then(|&i| batches.get_mut(i))
			&& b.status == BatchStatus::Committed
		{
			b.damaged = true; // an undo record of it may be the one that was lost
		}
		writing = Some(e.batch().to_owned());
		if let Entry::Begin { batch, at, source, reason, v, .. } = &e {
			index.insert(batch.clone(), batches.len());
			batches.push(BatchView {
				id: batch.clone(),
				at: *at,
				source: source.clone(),
				reason: reason.clone(),
				status: BatchStatus::Incomplete,
				damaged: *v > FORMAT,
				purged: false,
				steps: Vec::new(),
			});
			continue;
		}

		// A record without its `begin` (lost to damage): keep it visible as a damaged batch
		// instead of dropping it, so undo cannot silently skip over it. Not for a purge: it may
		// name a batch whose records are in the archive, and it never makes anything undoable.
		if !index.contains_key(e.batch()) {
			if matches!(e, Entry::Purge { .. }) {
				continue;
			}
			index.insert(e.batch().to_owned(), batches.len());
			batches.push(BatchView {
				id: e.batch().to_owned(),
				at: 0,
				source: String::new(),
				reason: "(its first journal record is unreadable)".into(),
				status: BatchStatus::Incomplete,
				damaged: true,
				purged: false,
				steps: Vec::new(),
			});
		}
		let Some(b) = index.get(e.batch()).and_then(|&i| batches.get_mut(i)) else { continue };
		match e {
			Entry::Intent { index, forward, inverse, .. } => {
				b.steps.push(Step {
					index,
					forward,
					inverse,
					done: false,
					published: false,
					check: None,
					mode: None,
					undo_published: false,
					failed: false,
					undone: false,
				});
			}
			Entry::Done { index, check, mode, .. } => {
				if let Some(s) = step_mut(&mut b.steps, index) {
					s.done = true;
					s.check = s.check.take().or(check);
					s.mode = mode.or(s.mode);
				}
			}
			Entry::Check { index, check, .. } => {
				if let Some(s) = step_mut(&mut b.steps, index) {
					s.check = Some(check);
				}
			}
			Entry::UndoPublished { index, .. } => {
				if let Some(s) = step_mut(&mut b.steps, index) {
					s.undo_published = true;
				}
			}
			Entry::Published { index, .. } => {
				if let Some(s) = step_mut(&mut b.steps, index) {
					s.published = true;
				}
			}
			Entry::UndoStep { index, .. } => {
				if let Some(s) = step_mut(&mut b.steps, index) {
					s.undone = true;
				}
			}
			Entry::Mode { index, mode, .. } => {
				if let Some(s) = step_mut(&mut b.steps, index) {
					s.mode = Some(mode);
				}
			}
			Entry::Failed { index, .. } => {
				if let Some(s) = step_mut(&mut b.steps, index) {
					s.failed = true;
				}
			}
			Entry::Commit { .. } => b.status = BatchStatus::Committed,
			Entry::Abort { .. } => b.status = BatchStatus::Aborted,
			Entry::Abandon { .. } => b.status = BatchStatus::Abandoned,
			Entry::Undo { .. } => b.status = BatchStatus::Undone,
			Entry::Purge { .. } => b.purged = true,
			Entry::Begin { .. } => {}
		}
	}
	batches
}

/// Every non-empty line of a journal in order, each with its record (`None` when unreadable).
fn read_records(mut reader: impl BufRead, corrupt: &mut usize) -> io::Result<Vec<(String, Option<Entry>)>> {
	let mut out = Vec::new();
	let mut buf = Vec::new();
	loop {
		buf.clear();
		// Bounded read: a line can never make us hold more than MAX_LINE bytes.
		let n = (&mut reader).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf)?;
		if n == 0 {
			break;
		}
		if buf.last() != Some(&b'\n') && buf.len() > MAX_LINE {
			// Oversized line: skip the rest of it without buffering it.
			loop {
				buf.clear();
				let m = (&mut reader).take(MAX_LINE as u64).read_until(b'\n', &mut buf)?;
				if m == 0 || buf.last() == Some(&b'\n') {
					break;
				}
			}
			*corrupt += 1;
			out.push((String::new(), None));
			continue;
		}
		// Invalid UTF-8 (e.g. a torn write cutting a multi-byte character) is a corrupt line,
		// not a reason to make the whole journal unreadable.
		let Ok(line) = std::str::from_utf8(&buf) else {
			*corrupt += 1;
			out.push((String::new(), None));
			continue;
		};
		let line = line.trim_end_matches(['\n', '\r']);
		if line.trim().is_empty() {
			continue;
		}
		let entry = parse_line(line);
		if entry.is_none() {
			*corrupt += 1;
		}
		out.push((line.to_owned(), entry));
	}
	Ok(out)
}

/// Open (creating it private) a file of the state folder: `fresh` empties it, otherwise appends.
fn private_file(path: &Path, fresh: bool) -> io::Result<File> {
	let mut options = OpenOptions::new();
	options.create(true).read(true);
	if fresh {
		options.write(true).truncate(true);
	} else {
		options.append(true);
	}
	#[cfg(unix)]
	std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
	let file = options.open(path)?;
	crate::path::check_private_file(path)?;
	Ok(file)
}

/// Lock `journal.lock` for this engine, or say that another one has it.
fn lock_file(path: &Path) -> io::Result<File> {
	crate::path::check_private_file(path)?;
	let file = private_file(path, false)?;
	match file.try_lock() {
		Ok(()) => Ok(file),
		Err(TryLockError::WouldBlock) => Err(io::Error::new(
			io::ErrorKind::WouldBlock,
			format!("journal is in use by another tpfs process: {}", path.with_extension("jsonl").display()),
		)),
		Err(TryLockError::Error(e)) => Err(e),
	}
}

/// Intents are written in step order, so step `i` is normally at position `i`: constant time for a
/// batch of 100 000 steps instead of a scan per record. Falls back to a search for anything unusual.
fn step_mut(steps: &mut [Step], index: usize) -> Option<&mut Step> {
	if steps.get(index).is_some_and(|s| s.index == index) {
		return steps.get_mut(index);
	}
	steps.iter_mut().find(|s| s.index == index)
}

/// Journal record format written by this version.
const FORMAT: u32 = 1;

fn format_v1() -> u32 {
	1
}

fn is_v1(v: &u32) -> bool {
	*v == 1
}

/// Longest journal line accepted. A record holds at most two operations and a reason, so anything
/// this long is damage, not data.
const MAX_LINE: usize = 1 << 20;

fn parse_line(line: &str) -> Option<Entry> {
	let (crc, json) = line.split_once(' ')?;
	let crc = u32::from_str_radix(crc, 16).ok()?;
	(crc32fast::hash(json.as_bytes()) == crc).then(|| serde_json::from_str(json).ok()).flatten()
}
