use std::{
	fs::{self, File, OpenOptions, TryLockError},
	io::{self, BufRead, BufReader, Seek, SeekFrom, Write},
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
pub enum Entry {
	Begin { batch: String, at: u64, source: String, reason: String, digest: String },
	Intent { batch: String, index: usize, forward: Op, inverse: Op },
	Done { batch: String, index: usize },
	Commit { batch: String },
	Abort { batch: String },
	UndoStep { batch: String, index: usize },
	Undo { batch: String },
}

impl Entry {
	fn batch(&self) -> &str {
		match self {
			Entry::Begin { batch, .. }
			| Entry::Intent { batch, .. }
			| Entry::Done { batch, .. }
			| Entry::Commit { batch }
			| Entry::Abort { batch }
			| Entry::UndoStep { batch, .. }
			| Entry::Undo { batch } => batch,
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchStatus {
	/// Started but neither committed nor aborted — interrupted by a crash; `recover` rolls it back.
	Incomplete,
	Committed,
	Aborted,
	Undone,
}

/// A batch reconstructed from the journal.
#[derive(Clone, Debug)]
pub struct BatchView {
	pub id: String,
	pub at: u64,
	pub source: String,
	pub reason: String,
	pub status: BatchStatus,
	/// (index, forward, inverse, done, undone)
	pub steps: Vec<Step>,
}

#[derive(Clone, Debug)]
pub struct Step {
	pub index: usize,
	pub forward: Op,
	pub inverse: Op,
	pub done: bool,
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
		if let Some(dir) = path.parent() {
			create_private_dir(dir)?;
		}

		let mut options = OpenOptions::new();
		options.create(true).append(true).read(true);
		#[cfg(unix)]
		std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
		let file = options.open(&path)?;

		match file.try_lock() {
			Ok(()) => {}
			Err(TryLockError::WouldBlock) => {
				return Err(io::Error::new(
					io::ErrorKind::WouldBlock,
					format!("journal is in use by another planfs process: {}", path.display()),
				));
			}
			Err(TryLockError::Error(e)) => return Err(e),
		}

		Ok(Self { path, file, corrupt: 0 })
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Number of unreadable lines seen by the last read (e.g. a torn write after a crash).
	pub fn corrupt_lines(&self) -> usize {
		self.corrupt
	}

	pub fn append(&mut self, entry: &Entry) -> io::Result<()> {
		let json = serde_json::to_string(entry).map_err(io::Error::other)?;
		let line = format!("{:08x} {json}\n", crc32fast::hash(json.as_bytes()));
		self.file.write_all(line.as_bytes())?;
		self.file.sync_data()
	}

	pub fn entries(&mut self) -> io::Result<Vec<Entry>> {
		// Read through the locked handle itself: on Windows the lock is mandatory, so a second
		// handle could not read the file. Appends still go to the end regardless of the position.
		let mut file = &self.file;
		file.seek(SeekFrom::Start(0))?;
		let reader = BufReader::new(file);
		let mut out = Vec::new();
		self.corrupt = 0;

		for line in reader.lines() {
			let line = line?;
			if line.trim().is_empty() {
				continue;
			}
			match parse_line(&line) {
				Some(e) => out.push(e),
				None => self.corrupt += 1,
			}
		}
		Ok(out)
	}

	/// All batches in journal order.
	pub fn batches(&mut self) -> io::Result<Vec<BatchView>> {
		let mut batches: Vec<BatchView> = Vec::new();
		for e in self.entries()? {
			if let Entry::Begin { batch, at, source, reason, .. } = &e {
				batches.push(BatchView {
					id: batch.clone(),
					at: *at,
					source: source.clone(),
					reason: reason.clone(),
					status: BatchStatus::Incomplete,
					steps: Vec::new(),
				});
				continue;
			}

			let Some(b) = batches.iter_mut().rev().find(|b| b.id == e.batch()) else { continue };
			match e {
				Entry::Intent { index, forward, inverse, .. } => {
					b.steps.push(Step { index, forward, inverse, done: false, undone: false });
				}
				Entry::Done { index, .. } => {
					if let Some(s) = step_mut(&mut b.steps, index) {
						s.done = true;
					}
				}
				Entry::UndoStep { index, .. } => {
					if let Some(s) = step_mut(&mut b.steps, index) {
						s.undone = true;
					}
				}
				Entry::Commit { .. } => b.status = BatchStatus::Committed,
				Entry::Abort { .. } => b.status = BatchStatus::Aborted,
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

fn parse_line(line: &str) -> Option<Entry> {
	let (crc, json) = line.split_once(' ')?;
	let crc = u32::from_str_radix(crc, 16).ok()?;
	(crc32fast::hash(json.as_bytes()) == crc).then(|| serde_json::from_str(json).ok()).flatten()
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::DirBuilderExt;
		fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
	}
	#[cfg(not(unix))]
	fs::create_dir_all(dir)
}
