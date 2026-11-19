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
pub enum Entry {
	Begin {
		batch: String,
		at: u64,
		source: String,
		reason: String,
		digest: String,
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
	},
	/// A copy built under a private name was published at its target: from here on the target is
	/// ours (written between `intent` and `done` by steps that copy).
	Published {
		batch: String,
		index: usize,
	},
	Commit {
		batch: String,
	},
	Abort {
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
	pub steps: Vec<Step>,
}

#[derive(Clone, Debug)]
pub struct Step {
	pub index: usize,
	pub forward: Op,
	pub inverse: Op,
	pub done: bool,
	/// The step's copy reached its target (see [`Entry::Published`]).
	pub published: bool,
	/// See [`Entry::Done`].
	pub check: Option<String>,
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
		if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
			crate::path::private_dir(dir)?;
		}
		crate::path::check_private_file(&path)?;

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
				continue;
			}
			// Invalid UTF-8 (e.g. a torn write cutting a multi-byte character) is a corrupt line,
			// not a reason to make the whole journal unreadable.
			let Ok(line) = std::str::from_utf8(&buf) else {
				self.corrupt += 1;
				continue;
			};
			let line = line.trim_end_matches(['\n', '\r']);
			if line.trim().is_empty() {
				continue;
			}
			match parse_line(line) {
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
					b.steps.push(Step {
						index,
						forward,
						inverse,
						done: false,
						published: false,
						check: None,
						undone: false,
					});
				}
				Entry::Done { index, check, .. } => {
					if let Some(s) = step_mut(&mut b.steps, index) {
						s.done = true;
						s.check = check;
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

/// Longest journal line accepted. A record holds at most two operations and a reason, so anything
/// this long is damage, not data.
const MAX_LINE: usize = 1 << 20;

fn parse_line(line: &str) -> Option<Entry> {
	let (crc, json) = line.split_once(' ')?;
	let crc = u32::from_str_radix(crc, 16).ok()?;
	(crc32fast::hash(json.as_bytes()) == crc).then(|| serde_json::from_str(json).ok()).flatten()
}
