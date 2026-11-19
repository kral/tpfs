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
		/// For `rmdir`: the folder's permission bits, restored by undo.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		mode: Option<u32>,
	},
	/// A copy's fingerprint recorded separately (reserved; read for forward compatibility).
	Check {
		batch: String,
		index: usize,
		check: String,
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
			| Entry::Commit { batch }
			| Entry::Abort { batch }
			| Entry::Abandon { batch }
			| Entry::UndoStep { batch, .. }
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
pub struct Journal {
	path: PathBuf,
	file: File,
	corrupt: usize,
}

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

		let mut journal = Self { path, file, corrupt: 0 };
		journal.seal_torn_tail()?;
		Ok(journal)
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
		// Read through the locked handle itself: on Windows the lock is mandatory, so a second
		// handle could not read the file. Appends still go to the end regardless of the position.
		let mut file = &self.file;
		file.seek(SeekFrom::Start(0))?;
		let mut reader = BufReader::new(file);
		let mut out = Vec::new();
		let mut buf = Vec::new();
		self.corrupt = 0;

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
				self.corrupt += 1;
				out.push(None);
				continue;
			}
			// Invalid UTF-8 (e.g. a torn write cutting a multi-byte character) is a corrupt line,
			// not a reason to make the whole journal unreadable.
			let Ok(line) = std::str::from_utf8(&buf) else {
				self.corrupt += 1;
				out.push(None);
				continue;
			};
			let line = line.trim_end_matches(['\n', '\r']);
			if line.trim().is_empty() {
				continue;
			}
			let entry = parse_line(line);
			if entry.is_none() {
				self.corrupt += 1;
			}
			out.push(entry);
		}
		Ok(out)
	}

	/// All batches in journal order.
	pub fn batches(&mut self) -> io::Result<Vec<BatchView>> {
		let mut batches: Vec<BatchView> = Vec::new();
		// Batch id -> position of its latest view: one lookup per record, not a scan of history.
		let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
		// Batches that were open when an unreadable line appeared, until the next readable record.
		let mut suspect: Vec<String> = Vec::new();
		for e in self.records()? {
			let Some(e) = e else {
				// Only the batch being written when the damage happened (the newest) can have lost
				// a record to it; an older interrupted batch is not affected by later damage.
				if let Some(b) = batches.last().filter(|b| b.status == BatchStatus::Incomplete)
					&& !suspect.contains(&b.id)
				{
					suspect.push(b.id.clone());
				}
				continue;
			};
			// Anything readable after the damage — except recovery closing a suspect batch, the normal
			// end of a crash with a torn last line — means records of the open batches may be lost
			// (an intent, a done, or the commit itself).
			if !suspect.is_empty() {
				// Records recovery itself writes for a suspect batch (it may need several runs).
				let closes_suspect = matches!(&e,
					Entry::Abort { batch } | Entry::UndoPublished { batch, .. } if suspect.contains(batch));
				if !closes_suspect {
					for b in batches.iter_mut().filter(|b| suspect.contains(&b.id)) {
						b.damaged = true;
					}
				}
				// After recovery's intermediate records the batch is still open: keep watching it.
				if !matches!(&e, Entry::UndoPublished { .. }) {
					suspect.clear();
				}
			}
			if let Entry::Begin { batch, at, source, reason, v, .. } = &e {
				index.insert(batch.clone(), batches.len());
				batches.push(BatchView {
					id: batch.clone(),
					at: *at,
					source: source.clone(),
					reason: reason.clone(),
					status: BatchStatus::Incomplete,
					damaged: *v > FORMAT,
					steps: Vec::new(),
				});
				continue;
			}

			// A record without its `begin` (lost to damage): keep it visible as a damaged batch
			// instead of dropping it, so undo cannot silently skip over it.
			if !index.contains_key(e.batch()) {
				index.insert(e.batch().to_owned(), batches.len());
				batches.push(BatchView {
					id: e.batch().to_owned(),
					at: 0,
					source: String::new(),
					reason: "(its first journal record is unreadable)".into(),
					status: BatchStatus::Incomplete,
					damaged: true,
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
						s.mode = mode;
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
				Entry::Failed { index, .. } => {
					if let Some(s) = step_mut(&mut b.steps, index) {
						s.failed = true;
					}
				}
				Entry::Commit { .. } => b.status = BatchStatus::Committed,
				Entry::Abort { .. } => b.status = BatchStatus::Aborted,
				Entry::Abandon { .. } => b.status = BatchStatus::Abandoned,
				Entry::Undo { .. } => b.status = BatchStatus::Undone,
				Entry::Begin { .. } => {}
			}
		}
		Ok(batches)
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
