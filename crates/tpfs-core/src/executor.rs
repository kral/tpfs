use std::{
	fmt, fs, io,
	path::{Path, PathBuf},
	time::{SystemTime, UNIX_EPOCH},
};

use crate::{
	fsops::{Fs, same_file, sync_parent},
	journal::{BatchStatus, Entry, Journal},
	op::{Op, Plan},
	path::{exists, normalize},
	validate::{Issue, Validator},
};

/// Who asked for a plan. Agents may only propose; see [`crate::risk`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
	Human,
	Agent,
}

impl Source {
	pub fn as_str(self) -> &'static str {
		match self {
			Source::Human => "human",
			Source::Agent => "agent",
		}
	}
}

#[derive(Debug)]
pub enum ExecError {
	/// The plan did not pass validation; nothing was touched.
	Rejected(Vec<Issue>),
	/// A step failed; the steps before it were rolled back (errors listed if rollback was incomplete).
	Failed { step: usize, error: String, rollback_errors: Vec<String> },
	/// The journal could not be written; execution stopped before touching the disk further.
	Journal(io::Error),
}

impl fmt::Display for ExecError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			ExecError::Rejected(issues) => {
				writeln!(f, "plan rejected:")?;
				for i in issues {
					writeln!(f, "  {i}")?;
				}
				Ok(())
			}
			ExecError::Failed { step, error, rollback_errors } if rollback_errors.is_empty() => {
				write!(f, "step #{} failed ({error}); previous steps were rolled back", step + 1)
			}
			ExecError::Failed { step, error, rollback_errors } => write!(
				f,
				"step #{} failed ({error}); rollback was incomplete: {}",
				step + 1,
				rollback_errors.join("; ")
			),
			ExecError::Journal(e) => write!(f, "journal error: {e}"),
		}
	}
}

impl std::error::Error for ExecError {}

impl From<io::Error> for ExecError {
	fn from(e: io::Error) -> Self {
		ExecError::Journal(e)
	}
}

#[derive(Clone, Debug)]
pub struct BatchReport {
	pub id: String,
	pub steps: usize,
}

#[derive(Clone, Debug, Default)]
pub struct RecoveryReport {
	pub rolled_back: Vec<String>,
	pub errors: Vec<String>,
}

/// Executes validated plans, step by step, through the write-ahead journal.
pub struct Executor<'a> {
	validator: &'a Validator,
	journal: &'a mut Journal,
	trash: PathBuf,
	hook: Option<Box<dyn FnMut(usize, Phase) + 'a>>,
}

/// Test hook phases: right before a step touches the disk (after its re-check and intent record),
/// and right after it did (before its `done` record).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
	BeforeApply,
	AfterApply,
}

impl<'a> Executor<'a> {
	pub fn new(validator: &'a Validator, journal: &'a mut Journal, trash: impl Into<PathBuf>) -> Self {
		Self { validator, journal, trash: trash.into(), hook: None }
	}

	/// Test hook, to simulate another program changing the file system mid-plan or a crash.
	#[doc(hidden)]
	pub fn hook(mut self, hook: impl FnMut(usize, Phase) + 'a) -> Self {
		self.hook = Some(Box::new(hook));
		self
	}

	/// Progress: called with the number of steps done after each step that changed the disk.
	pub fn on_step_done(self, mut f: impl FnMut(usize) + 'a) -> Self {
		self.hook(move |i, phase| {
			if phase == Phase::AfterApply {
				f(i + 1)
			}
		})
	}

	/// Test hook that runs only before a step touches the disk.
	#[doc(hidden)]
	pub fn before_apply(self, mut hook: impl FnMut(usize) + 'a) -> Self {
		self.hook(move |i, phase| {
			if phase == Phase::BeforeApply {
				hook(i)
			}
		})
	}

	pub fn execute(&mut self, plan: &Plan, source: Source) -> Result<BatchReport, ExecError> {
		let issues = self.validator.validate(plan);
		if !issues.is_empty() {
			return Err(ExecError::Rejected(issues));
		}

		let batch = new_batch_id();
		self.journal.append(&Entry::Begin {
			batch: batch.clone(),
			at: now_secs(),
			source: source.as_str().into(),
			reason: plan.reason.clone(),
			digest: plan.digest(),
		})?;

		let fs = Fs::confined(self.validator.roots());
		let mut applied: Vec<Op> = Vec::new();
		for (i, op) in plan.ops.iter().enumerate() {
			// Re-check this step against the disk as it is right now: earlier steps are already
			// applied, and the world may have changed since the plan was validated.
			let recheck = self.validator.validate(&Plan::new(vec![op.clone()], ""));
			let inverse = self.inverse(op, &batch, i);

			if let Some(issue) = recheck.into_iter().next() {
				let rollback_errors = self.revert(&fs, &applied, &batch);
				self.journal.append(&Entry::Abort { batch })?;
				return Err(ExecError::Failed { step: i, error: issue.message, rollback_errors });
			}

			self.journal.append(&Entry::Intent {
				batch: batch.clone(),
				index: i,
				forward: op.clone(),
				inverse: inverse.clone(),
			})?;
			if let Some(hook) = &mut self.hook {
				hook(i, Phase::BeforeApply);
			}

			match apply(op, &fs, &self.trash, &batch, i, Some(&inverse)) {
				Ok(()) => {
					if let Some(hook) = &mut self.hook {
						hook(i, Phase::AfterApply);
					}
					self.journal.append(&Entry::Done { batch: batch.clone(), index: i })?;
					applied.push(inverse);
				}
				Err(error) => {
					let mut rollback_errors = Vec::new();
					// A target that "already exists" belongs to someone else (it appeared after the
					// re-check): leave it alone. Otherwise the step may have left a partial result.
					if error.kind() != io::ErrorKind::AlreadyExists
						&& let Some(fix) = cleanup(op, &inverse)
						&& let Err(e) = apply(&fix, &fs, &self.trash, &format!("{batch}-rollback"), i, None)
					{
						rollback_errors.push(format!("{fix}: {e}"));
					}
					rollback_errors.extend(self.revert(&fs, &applied, &batch));
					self.journal.append(&Entry::Abort { batch })?;
					return Err(ExecError::Failed { step: i, error: error.to_string(), rollback_errors });
				}
			}
		}

		self.journal.append(&Entry::Commit { batch: batch.clone() })?;
		Ok(BatchReport { id: batch, steps: applied.len() })
	}

	/// Revert the most recent committed batch (made by anyone — a human or an agent).
	/// Returns `None` when there is nothing to undo. Partially undone batches resume where they stopped.
	pub fn undo_last(&mut self) -> Result<Option<BatchReport>, ExecError> {
		let Some(batch) = self.journal.batches()?.into_iter().rev().find(|b| b.status == BatchStatus::Committed) else {
			return Ok(None);
		};

		let mut errors = Vec::new();
		for step in batch.steps.iter().rev().filter(|s| s.done && !s.undone) {
			match apply(&step.inverse, &Fs::unconfined(), &self.trash, &format!("{}-undo", batch.id), step.index, None)
			{
				Ok(()) => self.journal.append(&Entry::UndoStep { batch: batch.id.clone(), index: step.index })?,
				Err(e) => {
					errors.push(format!("{}: {e}", step.inverse));
					break; // keep strict reverse order; the user can retry after fixing the cause
				}
			}
		}

		if !errors.is_empty() {
			return Err(ExecError::Failed { step: 0, error: "undo stopped".into(), rollback_errors: errors });
		}
		self.journal.append(&Entry::Undo { batch: batch.id.clone() })?;
		Ok(Some(BatchReport { id: batch.id, steps: batch.steps.len() }))
	}

	/// Roll back batches that were interrupted (begun but never committed or aborted).
	pub fn recover(&mut self) -> Result<RecoveryReport, ExecError> {
		let batches: Vec<_> =
			self.journal.batches()?.into_iter().filter(|b| b.status == BatchStatus::Incomplete).collect();
		self.recover_batches(batches)
	}

	/// Roll back the newest batch if, and only if, it was interrupted: the change "Undo" means.
	/// An older interrupted batch is left for an explicit `recover`, because rolling it back now
	/// would apply its inverses over everything that happened since.
	pub fn recover_last(&mut self) -> Result<RecoveryReport, ExecError> {
		let last = self.journal.batches()?.pop().filter(|b| b.status == BatchStatus::Incomplete);
		self.recover_batches(last.into_iter().collect())
	}

	fn recover_batches(&mut self, batches: Vec<crate::journal::BatchView>) -> Result<RecoveryReport, ExecError> {
		let mut report = RecoveryReport::default();
		for batch in batches {
			for step in batch.steps.iter().rev() {
				let fix = if step.done { Some(step.inverse.clone()) } else { cleanup(&step.forward, &step.inverse) };
				if let Some(fix) = fix
					&& let Err(e) =
						apply(&fix, &Fs::unconfined(), &self.trash, &format!("{}-recover", batch.id), step.index, None)
				{
					report.errors.push(format!("{fix}: {e}"));
				}
			}
			self.journal.append(&Entry::Abort { batch: batch.id.clone() })?;
			report.rolled_back.push(batch.id);
		}
		Ok(report)
	}

	fn revert(&self, fs: &Fs, inverses: &[Op], batch: &str) -> Vec<String> {
		inverses
			.iter()
			.enumerate()
			.rev()
			.filter_map(|(i, inv)| {
				apply(inv, fs, &self.trash, &format!("{batch}-rollback"), i, None).err().map(|e| format!("{inv}: {e}"))
			})
			.collect()
	}

	/// The operation that undoes `op`. Computed *before* `op` runs, so it can be journaled first.
	fn inverse(&self, op: &Op, batch: &str, index: usize) -> Op {
		let n = |p: &Path| normalize(p).unwrap_or_else(|| p.to_path_buf());
		match op {
			Op::Move { from, to } => Op::Move { from: n(to), to: n(from) },
			Op::Copy { to, .. } => Op::Trash { path: n(to) },
			Op::Rename { path, new_name } => {
				let path = n(path);
				let old = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
				Op::Rename { path: path.with_file_name(new_name), new_name: old }
			}
			Op::Trash { path } => Op::Move { from: trash_slot(&self.trash, batch, index, &n(path)), to: n(path) },
			Op::Mkdir { path } => Op::Rmdir { path: n(path) },
			Op::Rmdir { path } => Op::Mkdir { path: n(path) },
		}
	}
}

/// Perform one operation. Inverse operations of `trash` are plain moves out of the trash.
/// `planned` is the journaled inverse: a `trash` goes exactly to the slot recorded there.
fn apply(op: &Op, fs: &Fs, trash: &Path, batch: &str, index: usize, planned: Option<&Op>) -> io::Result<()> {
	let n = |p: &Path| normalize(p).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "relative path"));
	match op {
		Op::Move { from, to } => fs.move_any(&n(from)?, &n(to)?),
		Op::Copy { from, to } => {
			let to = n(to)?;
			fs.copy_any(&n(from)?, &to)?;
			sync_parent(&to);
			Ok(())
		}
		Op::Rename { path, new_name } => {
			let path = n(path)?;
			let to = path.with_file_name(new_name);
			if path != to && same_file(&path, &to) {
				// Only the letter case changes on a case-insensitive file system: go through a temporary name.
				let tmp = path.with_file_name(format!(".tpfs-case-{batch}-{index}"));
				fs.rename_noreplace(&path, &tmp)?;
				if let Err(e) = fs.rename_noreplace(&tmp, &to) {
					let _ = fs.rename_noreplace(&tmp, &path);
					return Err(e);
				}
			} else {
				fs.rename_noreplace(&path, &to)?;
			}
			sync_parent(&to);
			Ok(())
		}
		Op::Trash { path } => {
			let path = n(path)?;
			let slot = match planned {
				Some(Op::Move { from, .. }) => from.clone(),
				_ => trash_slot(trash, batch, index, &path),
			};
			fs::create_dir_all(slot.parent().expect("slot has a parent"))?;
			fs.move_any(&path, &slot)
		}
		Op::Mkdir { path } => {
			let path = n(path)?;
			fs.mkdir(&path)?;
			sync_parent(&path);
			Ok(())
		}
		Op::Rmdir { path } => {
			let path = n(path)?;
			fs.rmdir(&path)?;
			sync_parent(&path);
			Ok(())
		}
	}
}

/// What to do to clean up after `forward` when the journal has an `intent` but no `done`
/// (a crash or a failed step), judged from the disk. Targets never exist before a step runs
/// (validation guarantees it), so anything found there is ours.
fn cleanup(forward: &Op, inverse: &Op) -> Option<Op> {
	let n = |p: &Path| normalize(p).unwrap_or_else(|| p.to_path_buf());
	match (forward, inverse) {
		(Op::Copy { to, .. }, _) => exists(&n(to)).then(|| Op::Trash { path: n(to) }),
		(Op::Mkdir { path }, _) => exists(&n(path)).then(|| inverse.clone()),
		(Op::Rmdir { path }, _) => (!exists(&n(path))).then(|| inverse.clone()),
		// Move and trash: the inverse moves the target back to the source.
		(_, Op::Move { from: target, to: source }) => moved_back(&n(target), &n(source), inverse),
		// Rename: the inverse renames the new name back.
		(Op::Rename { path: source, .. }, Op::Rename { path: target, .. }) => {
			moved_back(&n(target), &n(source), inverse)
		}
		_ => None,
	}
}

fn moved_back(target: &Path, source: &Path, inverse: &Op) -> Option<Op> {
	if !exists(target) {
		None
	} else if exists(source) && !same_file(target, source) {
		// Both sides exist: a cross-device move stopped half way — drop the partial copy.
		Some(Op::Trash { path: target.to_path_buf() })
	} else {
		Some(inverse.clone())
	}
}

/// Where `path` goes when trashed. Prefer a trash directory on the same file system, so trashing is a
/// rename and not a copy: `<mount point>/.tpfs-trash-<uid>` (Linux), else the global trash.
fn trash_slot(trash: &Path, batch: &str, index: usize, path: &Path) -> PathBuf {
	let base = local_trash(trash, path).unwrap_or_else(|| trash.to_path_buf());
	base.join(batch).join(index.to_string()).join(path.file_name().unwrap_or_else(|| "root".as_ref()))
}

#[cfg(target_os = "linux")]
fn local_trash(trash: &Path, path: &Path) -> Option<PathBuf> {
	use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

	let dev = fs::symlink_metadata(path).ok()?.dev();
	let trash_dev = trash.ancestors().find_map(|a| fs::metadata(a).ok())?.dev();
	if trash_dev == dev {
		return None;
	}

	// The mount point: the highest ancestor still on the same device.
	let mut mount = path.parent()?;
	for a in path.ancestors().skip(1) {
		match fs::metadata(a) {
			Ok(m) if m.dev() == dev => mount = a,
			_ => break,
		}
	}

	let uid = rustix::process::getuid().as_raw();
	let dir = mount.join(format!(".tpfs-trash-{uid}"));
	let _ = fs::DirBuilder::new().mode(0o700).create(&dir);

	// Never use a trash someone else prepared (a symlink, another owner, readable by others).
	let m = fs::symlink_metadata(&dir).ok()?;
	(m.is_dir() && m.uid() == uid && m.permissions().mode() & 0o077 == 0).then_some(dir)
}

#[cfg(not(target_os = "linux"))]
fn local_trash(_: &Path, _: &Path) -> Option<PathBuf> {
	None
}

fn now_secs() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn new_batch_id() -> String {
	let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
	format!("{nanos:x}-{:x}", std::process::id())
}
