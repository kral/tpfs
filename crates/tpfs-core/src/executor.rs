use std::{
	fmt, fs, io,
	path::{Path, PathBuf},
	time::{SystemTime, UNIX_EPOCH},
};

use crate::{
	fsops::{Fs, merge_back, part_path, remove_part, same_file, sync_parent},
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
#[non_exhaustive]
pub enum ExecError {
	/// The plan did not pass validation; nothing was touched.
	Rejected(Vec<Issue>),
	/// A step failed; the steps before it were rolled back (errors listed if rollback was incomplete).
	Failed { step: usize, error: String, rollback_errors: Vec<String> },
	/// An interrupted or damaged batch has to be dealt with first (`recover`, or by hand).
	Blocked(String),
	/// Undo of `batch` stopped; the steps reverted so far stay reverted and `undo` resumes there.
	Undo { batch: String, error: String },
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
			ExecError::Undo { batch, error } => write!(f, "cannot undo batch {batch}: {error}"),
			ExecError::Blocked(why) => f.write_str(why),
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
#[non_exhaustive]
pub struct BatchReport {
	pub id: String,
	pub steps: usize,
}

#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct RecoveryReport {
	pub rolled_back: Vec<String>,
	pub errors: Vec<String>,
}

/// Executes validated plans, step by step, through the write-ahead journal.
pub struct Executor<'a> {
	/// The caller's validator, with tpfs's own state (journal folder, trash) protected on top.
	validator: Validator,
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
		// One spelling of the trash for every run: absolute and with no symlink on the way (a relative
		// or symlinked state folder must not make journaled trash paths unusable for undo).
		let trash = absolute_real(&trash.into());
		let state = journal.path().parent().map(Path::to_path_buf);
		let validator = validator.clone().with_protected(state.into_iter().chain([trash.clone()]));
		Self { validator, journal, trash, hook: None }
	}

	/// Test hook (feature `test-support`), to simulate another program changing the file system
	/// mid-plan or a crash.
	#[cfg(feature = "test-support")]
	pub fn hook(self, hook: impl FnMut(usize, Phase) + 'a) -> Self {
		self.set_hook(hook)
	}

	fn set_hook(mut self, hook: impl FnMut(usize, Phase) + 'a) -> Self {
		self.hook = Some(Box::new(hook));
		self
	}

	/// Progress: called with the number of steps done after each step that changed the disk.
	pub fn on_step_done(self, mut f: impl FnMut(usize) + 'a) -> Self {
		self.set_hook(move |i, phase| {
			if phase == Phase::AfterApply {
				f(i + 1)
			}
		})
	}

	/// Test hook (feature `test-support`) that runs only before a step touches the disk.
	#[cfg(feature = "test-support")]
	pub fn before_apply(self, mut hook: impl FnMut(usize) + 'a) -> Self {
		self.set_hook(move |i, phase| {
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

		// Never build on top of an interrupted batch: its rollback would later run over this one.
		// (An older one, below committed batches, is reported by recover and can be abandoned.)
		if let Some(b) = self
			.journal
			.batches()?
			.into_iter()
			.rev()
			.find(|b| !matches!(b.status, BatchStatus::Aborted | BatchStatus::Undone | BatchStatus::Abandoned))
			.filter(|b| b.status == BatchStatus::Incomplete)
		{
			return Err(ExecError::Blocked(format!(
				"batch {} was interrupted{}; run `tpfs recover` first",
				b.id,
				if b.damaged { " and its journal records are damaged" } else { "" }
			)));
		}

		let batch = new_batch_id();
		self.journal.write(&Entry::Begin {
			batch: batch.clone(),
			at: now_secs(),
			source: source.as_str().into(),
			reason: plan.reason.clone(),
			digest: plan.digest(),
			v: 1,
		})?;

		// Act on the paths as validated: spelled the way the file system spells them, under a root,
		// with no symlink on the way (the confined calls refuse any other spelling).
		let resolved = self.resolve_plan(plan);
		let fs = Fs::confined(self.validator.roots());
		let mut applied: Vec<Op> = Vec::new();
		for (i, op) in resolved.ops.iter().enumerate() {
			// Re-check this step against the disk as it is right now: earlier steps are already
			// applied, and the world may have changed since the plan was validated.
			let recheck = self.validator.validate(&Plan::new(vec![op.clone()], ""));
			let inverse = self.inverse(op, &batch, i);

			if let Some(issue) = recheck.into_iter().next() {
				let rollback_errors = self.roll_back(&batch)?;
				return Err(ExecError::Failed { step: i, error: issue.message, rollback_errors });
			}

			self.journal.write(&Entry::Intent {
				batch: batch.clone(),
				index: i,
				forward: op.clone(),
				inverse: inverse.clone(),
			})?;
			if let Some(hook) = &mut self.hook {
				hook(i, Phase::BeforeApply);
			}

			// A folder's own permissions, so undoing its removal restores them.
			let mode = match op {
				#[cfg(unix)]
				Op::Rmdir { path } => {
					use std::os::unix::fs::PermissionsExt;
					fs::symlink_metadata(path).ok().map(|m| m.permissions().mode() & 0o7777)
				}
				_ => None,
			};
			let published = std::cell::Cell::new(false);
			let result = {
				let journal = &mut self.journal;
				let mut publish = || {
					journal.write(&Entry::Published { batch: batch.clone(), index: i })?;
					published.set(true);
					Ok(())
				};
				apply(op, &fs, &self.trash, &batch, i, Some(&inverse), &mut publish)
			};
			match result {
				Ok(()) => {
					if let Some(hook) = &mut self.hook {
						hook(i, Phase::AfterApply);
					}
					// A copy's fingerprint right after it was made: undo reverts later steps first,
					// so at its own undo the copy is back in exactly this state unless the user changed it.
					let check = match op {
						Op::Copy { to, .. } => normalize(to).and_then(|t| fingerprint(&t)),
						_ => None,
					};
					self.journal.write(&Entry::Done { batch: batch.clone(), index: i, check, mode })?;
					applied.push(inverse);
				}
				Err(error) => {
					// Clean up what this step may have left, judged from the disk and from whether its
					// copy was published. A target that is not provably ours is left alone.
					let mut rollback_errors = Vec::new();
					// The step's own call failed, so it did nothing that the disk alone could prove: only
					// tpfs's own marks (a partial copy, a published copy, a case-rename's temporary
					// name) are cleaned up — never an entry that merely looks like this step's result.
					let ours = |f: &Fix| match f {
						Fix::Op(_) => published.get(),
						Fix::Conflict { .. } => false,
						_ => true,
					};
					for fix in cleanup(op, &inverse, &tag(&batch, i), published.get()).into_iter().filter(ours) {
						if let Err(e) = apply_fix(&fix, &fs, &self.trash, &format!("{batch}-recover"), i) {
							rollback_errors.push(clean(format!("{fix}: {e}")));
						}
					}
					if rollback_errors.is_empty() {
						self.journal.write(&Entry::Failed { batch: batch.clone(), index: i })?;
					}
					rollback_errors.extend(self.roll_back(&batch)?);
					return Err(ExecError::Failed { step: i, error: clean(format!("{op}: {error}")), rollback_errors });
				}
			}
		}

		self.journal.write(&Entry::Commit { batch: batch.clone() })?;
		Ok(BatchReport { id: batch, steps: applied.len() })
	}

	/// Revert the most recent committed batch (made by anyone — a human or an agent).
	/// Returns `None` when there is nothing to undo. Partially undone batches resume where they stopped.
	///
	/// Refuses while a newer batch is interrupted (it has to be recovered first), and stops — keeping
	/// the user's data — when a copy was changed after it was made. Every journaled path must still
	/// be reachable without symlinks below the validator's roots, or in the trash.
	pub fn undo_last(&mut self) -> Result<Option<BatchReport>, ExecError> {
		let batches = self.journal.batches()?;
		if let Some(b) = batches
			.iter()
			.rev()
			.find(|b| !matches!(b.status, BatchStatus::Aborted | BatchStatus::Undone | BatchStatus::Abandoned))
			&& b.status == BatchStatus::Incomplete
		{
			return Err(ExecError::Undo {
				batch: b.id.clone(),
				error: if b.damaged {
					format!(
						"its journal records are damaged; see `tpfs log`, and if you accept the disk as it is: `tpfs recover --abandon {}`",
						b.id
					)
				} else {
					"it was interrupted and must be rolled back first (`tpfs recover`)".into()
				},
			});
		}
		let Some(batch) = batches.into_iter().rev().find(|b| b.status == BatchStatus::Committed) else {
			return Ok(None);
		};

		if batch.damaged {
			return Err(ExecError::Undo {
				batch: batch.id,
				error: "its journal records are damaged; undo it by hand from `tpfs log`".into(),
			});
		}
		let fs = Fs::confined(self.validator.roots());
		let mut reverted = 0;
		for step in batch.steps.iter().rev().filter(|s| s.done && !s.undone) {
			let fail = |error: String| ExecError::Undo { batch: batch.id.clone(), error };
			self.check_journaled(&step.inverse).map_err(fail)?;
			self.copy_unchanged(step).map_err(fail)?;
			self.replay(&fs, &batch.id, "undo", step).map_err(|e| fail(clean(format!("{}: {e}", step.inverse))))?;
			self.journal.write(&Entry::UndoStep { batch: batch.id.clone(), index: step.index })?;
			reverted += 1;
		}

		self.journal.write(&Entry::Undo { batch: batch.id.clone() })?;
		Ok(Some(BatchReport { id: batch.id, steps: reverted }))
	}

	/// The plan with every path replaced by its validated form (it passed validation, so each one
	/// resolves; a path that no longer does is kept and fails its per-step re-check).
	fn resolve_plan(&self, plan: &Plan) -> Plan {
		let r = |p: &PathBuf| self.validator.check_path(p).unwrap_or_else(|_| p.clone());
		let ops = plan
			.ops
			.iter()
			.map(|op| match op {
				Op::Move { from, to } => Op::Move { from: r(from), to: r(to) },
				Op::Copy { from, to } => Op::Copy { from: r(from), to: r(to) },
				Op::Rename { path, new_name } => Op::Rename { path: r(path), new_name: new_name.clone() },
				Op::Trash { path } => Op::Trash { path: r(path) },
				Op::Mkdir { path } => Op::Mkdir { path: r(path) },
				Op::Rmdir { path } => Op::Rmdir { path: r(path) },
			})
			.collect();
		Plan { ops, reason: plan.reason.clone() }
	}

	/// A copy changed since its batch finished is the user's now: undo and recovery keep it.
	fn copy_unchanged(&self, step: &crate::journal::Step) -> Result<(), String> {
		if let (Op::Copy { to, .. }, Some(check)) = (&step.forward, &step.check)
			&& let Some(to) = normalize(to)
			&& exists(&to)
			&& fingerprint(&to).as_ref() != Some(check)
		{
			return Err(format!(
				"{} was changed after it was copied; it is kept. Move it away (or trash it yourself) and try again.",
				crate::sanitize(to.as_os_str())
			));
		}
		Ok(())
	}

	/// Apply a committed step's inverse (undo or recovery), resuming whatever an earlier, interrupted
	/// attempt left: a partial copy is dropped and redone, a cross-device move whose copy was
	/// already published is finished, a letter-case rename stuck at its temporary name is completed,
	/// and an inverse that already took effect is skipped.
	fn replay(&mut self, fs: &Fs, batch: &str, what: &str, step: &crate::journal::Step) -> io::Result<()> {
		let label = format!("{batch}-{what}");
		let t = tag(&label, step.index);
		let n = |p: &Path| normalize(p).unwrap_or_else(|| p.to_path_buf());
		match &step.inverse {
			Op::Move { from, to } => {
				let part = part_path(&n(to), &t);
				if step.undo_published && !exists(&part) && exists(&n(from)) && exists(&n(to)) {
					// An earlier attempt published its copy (the part is gone, renamed into place):
					// what is left at the old place duplicates it.
					return apply(&Op::Trash { path: n(from) }, fs, &self.trash, &label, step.index, None, &mut || {
						Ok(())
					});
				}
				remove_part(fs, &part)?;
			}
			Op::Trash { path } => {
				let slot = trash_slot(&self.trash, &label, step.index, &n(path));
				let part = part_path(&slot, &t);
				if step.undo_published && !exists(&part) && exists(&slot) && exists(&n(path)) {
					// An earlier attempt got the item into its trash slot and stopped while removing
					// the original: what is left duplicates it; it goes to a trash slot of its own.
					let rest = format!("{label}-rest");
					return apply(&Op::Trash { path: n(path) }, fs, &self.trash, &rest, step.index, None, &mut || {
						Ok(())
					});
				}
				remove_part(fs, &part)?;
			}
			Op::Rename { path, new_name } => {
				let tmp = case_tmp(&n(path), &t);
				if exists(&tmp) {
					fs.rename_noreplace(&tmp, &n(path).with_file_name(new_name))?;
					return Ok(());
				}
			}
			_ => {}
		}
		if already_applied(&step.inverse) {
			return Ok(());
		}
		let journal = &mut self.journal;
		let mut publish = || journal.write(&Entry::UndoPublished { batch: batch.to_owned(), index: step.index });
		apply(&step.inverse, fs, &self.trash, &label, step.index, None, &mut publish)?;
		// Restore a removed folder's own permissions, not the default ones.
		#[cfg(unix)]
		if let (Op::Mkdir { path }, Some(mode)) = (&step.inverse, step.mode) {
			use std::os::unix::fs::PermissionsExt;
			crate::fsops::set_dir_mode_at(fs, &n(path), fs::Permissions::from_mode(mode))?;
		}
		Ok(())
	}

	/// A journaled operation replayed by undo or recovery must stay where tpfs may act: below the
	/// validator's roots without symlinks, or in a trash. A forged or stale journal entry pointing
	/// through a planted symlink is refused instead of followed.
	fn check_journaled(&self, op: &Op) -> Result<(), String> {
		for p in op.paths() {
			let Some(p) = normalize(p) else { return Err(format!("journaled path is not absolute: {}", op)) };
			let in_trash = p.starts_with(&self.trash) || in_local_trash(&p);
			let checked = if in_trash { Ok(p) } else { self.validator.check_path(&p) };
			checked?;
		}
		Ok(())
	}

	/// Roll back batches that were interrupted (begun but never committed or aborted).
	///
	/// An interrupted batch with committed batches after it is not rolled back automatically: its
	/// inverses would run over those later changes. It is reported instead.
	pub fn recover(&mut self) -> Result<RecoveryReport, ExecError> {
		let all = self.journal.batches()?;
		let mut report = RecoveryReport::default();
		let mut batches = Vec::new();
		for (i, b) in all.iter().enumerate() {
			if b.status != BatchStatus::Incomplete {
				continue;
			}
			if all[i + 1..].iter().any(|later| later.status == BatchStatus::Committed) {
				report.errors.push(format!(
					"batch {} was interrupted before newer changes were made; it is not rolled back automatically (see `tpfs log`; to accept the disk as it is: `tpfs recover --abandon {}`)",
					b.id, b.id
				));
			} else {
				batches.push(b.clone());
			}
		}
		let mut rest = self.recover_batches(batches)?;
		report.rolled_back.append(&mut rest.rolled_back);
		report.errors.append(&mut rest.errors);
		Ok(report)
	}

	/// Give up on an interrupted (or damaged) batch without rolling it back: record it as aborted, leaving the
	/// disk as it is. For a batch recovery cannot handle (damaged records, or newer changes on top);
	/// the caller should have shown the user what it did (`tpfs log`).
	pub fn abandon(&mut self, id: &str) -> Result<(), ExecError> {
		let batches = self.journal.batches()?;
		match batches.iter().find(|b| b.id == id) {
			Some(b) if b.status == BatchStatus::Incomplete || (b.damaged && b.status == BatchStatus::Committed) => {
				self.journal.write(&Entry::Abandon { batch: id.to_owned() })?;
				Ok(())
			}
			Some(_) => Err(ExecError::Blocked(format!(
				"batch {} is neither interrupted nor damaged; nothing to abandon",
				crate::sanitize(id.as_ref())
			))),
			None => Err(ExecError::Blocked(format!("no batch {} in the journal", crate::sanitize(id.as_ref())))),
		}
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
		let fs = Fs::confined(self.validator.roots());
		// Newest first: an older batch's inverses assume the later ones were undone already.
		for batch in batches.into_iter().rev() {
			if batch.damaged {
				report
					.errors
					.push(format!("batch {}: journal records are damaged; not rolled back automatically", batch.id));
				continue;
			}
			let mut errors = Vec::new();
			for step in batch.steps.iter().rev() {
				if step.done {
					let result = self
						.check_journaled(&step.inverse)
						.and_then(|()| self.copy_unchanged(step))
						.map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))
						.and_then(|()| self.replay(&fs, &batch.id, "recover", step));
					if let Err(e) = result {
						errors.push(clean(format!("{}: {e}", step.inverse)));
					}
					continue;
				}
				if step.failed {
					continue; // its own call failed and its marks were cleaned up
				}
				for fix in cleanup(&step.forward, &step.inverse, &tag(&batch.id, step.index), step.published) {
					let result = match &fix {
						Fix::Op(op) => {
							self.check_journaled(op).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))
						}
						_ => Ok(()),
					}
					.and_then(|()| apply_fix(&fix, &fs, &self.trash, &format!("{}-recover", batch.id), step.index));
					if let Err(e) = result {
						errors.push(clean(format!("{fix}: {e}")));
					}
				}
			}
			if errors.is_empty() {
				self.journal.write(&Entry::Abort { batch: batch.id.clone() })?;
				report.rolled_back.push(batch.id);
			} else {
				// Left incomplete: fix the cause and run recover again; finished steps are skipped.
				report.errors.extend(errors);
			}
		}
		Ok(report)
	}

	/// Roll back the batch being executed after a step failed, exactly as `recover` would after a
	/// crash at this point — journaled the same way, so a crash during the rollback itself is resumed
	/// by the next `recover`. On errors the batch stays incomplete (the user runs `tpfs recover`).
	fn roll_back(&mut self, batch: &str) -> Result<Vec<String>, ExecError> {
		let Some(view) = self.journal.batches()?.into_iter().rev().find(|b| b.id == batch) else {
			return Ok(vec![format!("batch {batch} is missing from the journal")]);
		};
		Ok(self.recover_batches(vec![view])?.errors)
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

/// An absolute spelling of `path` without symlinks: its existing part canonicalised.
fn absolute_real(path: &Path) -> PathBuf {
	let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
	let mut rest = Vec::new();
	let mut base = abs.as_path();
	loop {
		if let Ok(c) = crate::path::canonicalize(base) {
			let mut out = c;
			out.extend(rest.iter().rev());
			return out;
		}
		match (base.parent(), base.file_name()) {
			(Some(p), Some(n)) => {
				rest.push(n);
				base = p;
			}
			_ => return abs,
		}
	}
}

/// Whether `path` lies in a genuine per-file-system trash of this user: a `.tpfs-trash-<uid>`
/// folder that is a real directory owned by the user and private. A path that merely contains such
/// a name (a forged journal entry) does not count.
fn in_local_trash(path: &Path) -> bool {
	#[cfg(unix)]
	{
		use std::os::unix::fs::{MetadataExt, PermissionsExt};
		let uid = rustix::process::getuid().as_raw();
		let name = format!(".tpfs-trash-{uid}");
		path.ancestors().any(|a| {
			a.file_name().is_some_and(|n| n == name.as_str())
				&& fs::symlink_metadata(a)
					.is_ok_and(|m| m.is_dir() && m.uid() == uid && m.permissions().mode() & 0o077 == 0)
		})
	}
	#[cfg(not(unix))]
	{
		let _ = path;
		false
	}
}

/// Error text for people and agents: io errors embed raw paths, which may carry terminal escapes.
fn clean(text: String) -> String {
	crate::sanitize(std::ffi::OsStr::new(&text))
}

/// Whether `op` (an inverse) has already taken effect, so replaying it after a crash is skipped.
fn already_applied(op: &Op) -> bool {
	let n = |p: &Path| normalize(p).unwrap_or_else(|| p.to_path_buf());
	match op {
		Op::Move { from, to } => !exists(&n(from)) && exists(&n(to)),
		Op::Trash { path } => !exists(&n(path)),
		Op::Rmdir { path } => !exists(&n(path)),
		Op::Mkdir { path } => n(path).is_dir(),
		Op::Rename { path, new_name } => {
			let path = n(path);
			!exists(&path) && exists(&path.with_file_name(new_name))
		}
		Op::Copy { .. } => false,
	}
}

/// A compact description of what a copy produced (names, kinds, sizes, modification times), so
/// undo can tell whether someone changed it since.
fn fingerprint(path: &Path) -> Option<String> {
	use sha2::{Digest, Sha256};
	fn walk(path: &Path, rel: &Path, h: &mut Sha256, budget: &mut usize) -> Option<()> {
		*budget = budget.checked_sub(1)?;
		let m = fs::symlink_metadata(path).ok()?;
		// Folder times are left out: adding and removing entries (by later steps and their undo)
		// changes them without changing anything the user made.
		let mtime = if m.is_dir() {
			0
		} else {
			m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())
		};
		h.update(format!("{}\0{}\0{}\0{mtime}\n", rel.display(), m.is_dir(), if m.is_dir() { 0 } else { m.len() }));
		if m.is_dir() {
			let mut names: Vec<_> = fs::read_dir(path).ok()?.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
			names.sort();
			for name in names {
				walk(&path.join(&name), &rel.join(&name), h, budget)?;
			}
		}
		Some(())
	}
	let mut h = Sha256::new();
	// Very large trees are not fingerprinted (undo then trashes them, which is still undoable).
	let mut budget = 100_000;
	walk(path, Path::new(""), &mut h, &mut budget)?;
	Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Private tag of one step, used for its in-progress names.
fn tag(batch: &str, index: usize) -> String {
	format!("{batch}-{index}")
}

/// Perform one operation. Inverse operations of `trash` are plain moves out of the trash.
/// `planned` is the journaled inverse: a `trash` goes exactly to the slot recorded there.
/// `publish` is called when a copy has been published at its target (see [`Entry::Published`]).
fn apply(
	op: &Op,
	fs: &Fs,
	trash: &Path,
	batch: &str,
	index: usize,
	planned: Option<&Op>,
	publish: &mut dyn FnMut() -> io::Result<()>,
) -> io::Result<()> {
	let n = |p: &Path| normalize(p).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "relative path"));
	let tag = tag(batch, index);
	match op {
		Op::Move { from, to } => {
			let to = n(to)?;
			fs.move_any(&n(from)?, &to, &part_path(&to, &tag), publish)
		}
		Op::Copy { from, to } => {
			let to = n(to)?;
			fs.copy_to(&n(from)?, &to, &part_path(&to, &tag), publish)
		}
		Op::Rename { path, new_name } => {
			let path = n(path)?;
			let to = path.with_file_name(new_name);
			if path != to && same_file(&path, &to) {
				// Only the letter case changes on a case-insensitive file system: go through a temporary name.
				let tmp = case_tmp(&path, &tag);
				fs.rename_noreplace(&path, &tmp)?;
				if let Err(e) = fs.rename_noreplace(&tmp, &to) {
					return match fs.rename_noreplace(&tmp, &path) {
						Ok(()) => Err(e),
						Err(back) => Err(io::Error::new(
							e.kind(),
							format!("{e}; the file is left at {} ({back})", tmp.display()),
						)),
					};
				}
			} else {
				fs.rename_noreplace(&path, &to)?;
			}
			sync_parent(&to);
			Ok(())
		}
		Op::Trash { path } => {
			let path = n(path)?;
			let (slot, tag) = match planned {
				Some(Op::Move { from, .. }) => (from.clone(), tag),
				// Clean-up trashes (undo, recovery) are not journaled: one that was interrupted is
				// resumed by taking a fresh slot when its own is taken (its partial copy is dropped
				// first), so running it again always converges and never needs to replace anything.
				_ => {
					let mut k = 0usize;
					loop {
						let label = if k == 0 { batch.to_owned() } else { format!("{batch}-{k}") };
						let slot = trash_slot(trash, &label, index, &path);
						let t = self::tag(&label, index);
						remove_part(fs, &part_path(&slot, &t))?;
						if !exists(&slot) {
							break (slot, t);
						}
						k += 1;
					}
				}
			};
			let parent = slot.parent().ok_or_else(|| io::Error::other("trash slot without a parent"))?;
			fs::create_dir_all(parent)?;
			// Make the new slot folders durable: trash/<batch> in trash, <index> in trash/<batch>.
			if let Some(batch_dir) = parent.parent() {
				sync_parent(batch_dir);
			}
			sync_parent(parent);
			fs.move_any(&path, &slot, &part_path(&slot, &tag), publish)
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

fn case_tmp(path: &Path, tag: &str) -> PathBuf {
	path.with_file_name(format!(".tpfs-case-{tag}"))
}

/// One clean-up action after a step that did not finish.
#[derive(Debug)]
enum Fix {
	Op(Op),
	/// Delete a partial copy under its private name.
	RemovePart(PathBuf),
	/// A cross-device move stopped while removing the source: put back what was removed.
	MergeBack {
		target: PathBuf,
		source: PathBuf,
	},
	/// Both the step's source and its target exist and they are different files: either the step
	/// never ran and someone created the target, or it ran and someone reused the source name. Which
	/// one cannot be told from the disk, so nothing is touched and the batch stays incomplete.
	Conflict {
		target: PathBuf,
		source: PathBuf,
	},
	/// A letter-case rename stopped between its two renames.
	RestoreCase {
		tmp: PathBuf,
		path: PathBuf,
	},
}

impl fmt::Display for Fix {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Fix::Op(op) => write!(f, "{op}"),
			Fix::RemovePart(p) => write!(f, "REMOVE PARTIAL COPY {}", crate::sanitize(p.as_os_str())),
			Fix::MergeBack { target, source } => write!(
				f,
				"RESTORE {} FROM {}",
				crate::sanitize(source.as_os_str()),
				crate::sanitize(target.as_os_str())
			),
			Fix::Conflict { target, source } => write!(
				f,
				"CHECK {} AND {} (both exist; tell which is which, then `tpfs recover --abandon <batch>`)",
				crate::sanitize(source.as_os_str()),
				crate::sanitize(target.as_os_str())
			),
			Fix::RestoreCase { tmp, path } => {
				write!(f, "RENAME {} BACK TO {}", crate::sanitize(tmp.as_os_str()), crate::sanitize(path.as_os_str()))
			}
		}
	}
}

fn apply_fix(fix: &Fix, fs: &Fs, trash: &Path, batch: &str, index: usize) -> io::Result<()> {
	match fix {
		Fix::Op(op) => apply(op, fs, trash, batch, index, None, &mut || Ok(())),
		Fix::RemovePart(part) => remove_part(fs, part),
		Fix::MergeBack { target, source } => merge_back(fs, target, source, &tag(batch, index), &mut |rest| {
			apply(&Op::Trash { path: rest.to_path_buf() }, fs, trash, batch, index, None, &mut || Ok(()))
		}),
		Fix::RestoreCase { tmp, path } => fs.rename_noreplace(tmp, path),
		Fix::Conflict { .. } => Err(io::Error::new(
			io::ErrorKind::AlreadyExists,
			"cannot tell from the disk whether this step took effect; nothing was changed",
		)),
	}
}

/// How to clean up after `forward` when the journal has an `intent` but no `done` (a crash or a
/// failed step), judged from the disk. Something found at a target is only touched when it is
/// provably ours: the source is gone (a rename moved it), or the step's copy was `published`.
fn cleanup(forward: &Op, inverse: &Op, tag: &str, published: bool) -> Vec<Fix> {
	let n = |p: &Path| normalize(p).unwrap_or_else(|| p.to_path_buf());
	let mut fixes = Vec::new();
	match (forward, inverse) {
		(Op::Copy { to, .. }, _) => {
			let to = n(to);
			let part = part_path(&to, tag);
			if exists(&part) {
				// Not renamed into place: whatever is at `to` is someone else's.
				fixes.push(Fix::RemovePart(part));
			} else if published && exists(&to) {
				fixes.push(Fix::Op(Op::Trash { path: to }));
			}
		}
		(Op::Mkdir { path }, _) => {
			if exists(&n(path)) {
				fixes.push(Fix::Op(inverse.clone()));
			}
		}
		(Op::Rmdir { path }, _) => {
			if !exists(&n(path)) {
				fixes.push(Fix::Op(inverse.clone()));
			}
		}
		// Move and trash: the inverse moves the target back to the source.
		(_, Op::Move { from: target, to: source }) => {
			let (target, source) = (n(target), n(source));
			let part = part_path(&target, tag);
			if exists(&part) {
				// Still copying: the source is untouched.
				fixes.push(Fix::RemovePart(part));
			} else if exists(&target) {
				if !exists(&source) || same_file(&target, &source) {
					fixes.push(Fix::Op(inverse.clone()));
				} else if published {
					fixes.push(Fix::MergeBack { target, source });
				} else {
					fixes.push(Fix::Conflict { target, source });
				}
			}
		}
		// Rename: the inverse renames the new name back.
		(Op::Rename { path: source, .. }, Op::Rename { path: target, .. }) => {
			let (target, source) = (n(target), n(source));
			let tmp = case_tmp(&source, tag);
			if exists(&tmp) {
				fixes.push(Fix::RestoreCase { tmp, path: source });
			} else if exists(&target) && (!exists(&source) || same_file(&target, &source)) {
				fixes.push(Fix::Op(inverse.clone()));
			} else if exists(&target) && target != source {
				fixes.push(Fix::Conflict { target, source });
			}
		}
		_ => {}
	}
	fixes
}

/// Where `path` goes when trashed. Prefer a trash directory on the same file system, so trashing is a
/// rename and not a copy: `<mount point>/.tpfs-trash-<uid>` (Unix), else the global trash (Windows:
/// trashing on another drive copies to the home drive first).
fn trash_slot(trash: &Path, batch: &str, index: usize, path: &Path) -> PathBuf {
	let base = local_trash(trash, path).unwrap_or_else(|| trash.to_path_buf());
	base.join(batch).join(index.to_string()).join(path.file_name().unwrap_or_else(|| "root".as_ref()))
}

#[cfg(unix)]
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

#[cfg(not(unix))]
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
