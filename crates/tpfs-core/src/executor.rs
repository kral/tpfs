use std::{
	fmt, fs, io,
	path::{Path, PathBuf},
	time::{SystemTime, UNIX_EPOCH},
};

use crate::{
	fsops::{Fs, make_dir_writable, merge_back, part_path, remove_part, same_file, sync_parent},
	journal::{BatchStatus, Entry, Journal},
	op::{Op, Plan},
	path::{exists, normalize},
	validate::{Issue, Validator},
};

/// Why a batch whose trash was emptied is not undone.
pub const PURGED: &str = "its trash was emptied (`tpfs trash empty`), so what it moved there is gone for good; \
	it can no longer be undone, and neither can the batches before it";

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
	/// Stopped on request (see [`Executor::cancel_on`]) before step `step`; the steps before it
	/// were rolled back (errors listed if rollback was incomplete).
	Cancelled { step: usize, rollback_errors: Vec<String> },
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
				"step #{} failed ({error}); rollback was incomplete: {} — fix the cause, then run `tpfs recover`",
				step + 1,
				rollback_errors.join("; ")
			),
			ExecError::Undo { batch, error } => {
				write!(f, "cannot undo batch {}: {error}", crate::sanitize(batch.as_ref()))
			}
			ExecError::Cancelled { step, rollback_errors } if rollback_errors.is_empty() => {
				write!(
					f,
					"stopped on request before step #{}; nothing was changed (earlier steps were rolled back)",
					step + 1
				)
			}
			ExecError::Cancelled { step, rollback_errors } => write!(
				f,
				"stopped on request before step #{}; rollback was incomplete: {} — fix the cause, then run `tpfs recover`",
				step + 1,
				rollback_errors.join("; ")
			),
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
	/// Called with the number of steps done (or undone) so far, after each one.
	progress: Option<Box<dyn FnMut(usize) + 'a>>,
	/// Set by another thread to stop before the next step (see [`Executor::cancel_on`]).
	cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

/// Test hook phases: right before a step touches the disk (after its re-check and intent record),
/// and right after it did (before its `done` record).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Phase {
	BeforeApply,
	AfterApply,
	/// A failed step was recorded as failed; its own marks are about to be cleaned up.
	FailedRecorded,
}

impl<'a> Executor<'a> {
	pub fn new(validator: &'a Validator, journal: &'a mut Journal, trash: impl Into<PathBuf>) -> Self {
		// One spelling of the trash for every run: absolute and with no symlink on the way (a relative
		// or symlinked state folder must not make journaled trash paths unusable for undo).
		let trash = absolute_real(&trash.into());
		let state = journal.path().parent().map(Path::to_path_buf);
		let validator = validator.clone().with_protected(state.into_iter().chain([trash.clone()]));
		Self { validator, journal, trash, hook: None, progress: None, cancel: None }
	}

	/// Test hook (feature `test-support`), to simulate another program changing the file system
	/// mid-plan or a crash.
	#[cfg(feature = "test-support")]
	pub fn hook(self, hook: impl FnMut(usize, Phase) + 'a) -> Self {
		self.set_hook(hook)
	}

	#[cfg(feature = "test-support")]
	fn set_hook(mut self, hook: impl FnMut(usize, Phase) + 'a) -> Self {
		self.hook = Some(Box::new(hook));
		self
	}

	/// Progress: called with the number of steps done (by `execute`) or reverted (by undo) so far,
	/// after each one is journaled.
	pub fn on_step_done(mut self, f: impl FnMut(usize) + 'a) -> Self {
		self.progress = Some(Box::new(f));
		self
	}

	/// Stop when `flag` is set, before the next step: `execute` then rolls the batch back (a batch
	/// is all or nothing) and returns [`ExecError::Cancelled`]; an undo keeps what it reverted and
	/// returns [`ExecError::Undo`] — undo again to finish it. A step already running is finished.
	pub fn cancel_on(mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
		self.cancel = Some(flag);
		self
	}

	fn cancelled(&self) -> bool {
		self.cancel.as_ref().is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
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
			let id = crate::sanitize(b.id.as_ref());
			return Err(ExecError::Blocked(if b.damaged {
				format!(
					"batch {id} was interrupted and its journal records are damaged; see `tpfs log {id}`, then `tpfs recover --abandon {id}` to accept the disk as it is"
				)
			} else {
				format!("batch {id} was interrupted; run `tpfs recover` first")
			}));
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
		let fs = self.fs(&resolved.ops);
		let mut applied: Vec<Op> = Vec::new();
		for (i, op) in resolved.ops.iter().enumerate() {
			// Re-check this step against the disk as it is right now: earlier steps are already
			// applied, and the world may have changed since the plan was validated.
			if self.cancelled() {
				let rollback_errors = self.roll_back(&batch)?;
				return Err(ExecError::Cancelled { step: i, rollback_errors });
			}
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
			// A folder's own permissions, so undoing its removal restores them; journaled before
			// the folder goes, so a crash right after the removal does not lose them.
			let mode = match op {
				#[cfg(unix)]
				Op::Rmdir { path } => {
					use std::os::unix::fs::PermissionsExt;
					fs::symlink_metadata(path).ok().map(|m| m.permissions().mode() & 0o7777)
				}
				_ => None,
			};
			if let Some(mode) = mode {
				self.journal.write(&Entry::Mode { batch: batch.clone(), index: i, mode })?;
			}
			if let Some(hook) = &mut self.hook {
				hook(i, Phase::BeforeApply);
			}
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
					// The mode is journaled once, in `Mode` before the removal (older journals also
					// carry it here; both are read).
					self.journal.write(&Entry::Done { batch: batch.clone(), index: i, check, mode: None })?;
					applied.push(inverse);
					if let Some(progress) = &mut self.progress {
						progress(i + 1);
					}
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
					let fixes: Vec<Fix> =
						cleanup(op, &inverse, &tag(&batch, i), published.get()).into_iter().filter(ours).collect();
					// A part that was not renamed into place is the proof that the target is not ours. Say the
					// step failed *before* removing it: once it is gone, "published and no part" would
					// read as "the target is ours" to a recovery after a crash right here.
					let only_parts = fixes.iter().all(|f| matches!(f, Fix::RemovePart(_)));
					if only_parts {
						self.journal.write(&Entry::Failed { batch: batch.clone(), index: i })?;
						if let Some(hook) = &mut self.hook {
							hook(i, Phase::FailedRecorded);
						}
					}
					for fix in &fixes {
						if let Err(e) = apply_fix(fix, &fs, &self.trash, &format!("{batch}-recover"), i) {
							rollback_errors.push(clean(format!("{fix}: {e}")));
						}
					}
					if !only_parts && rollback_errors.is_empty() {
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
		self.undo_newest(None)
	}

	/// Undo the newest committed batch only if it is `expected`, the one the user was shown: a
	/// batch committed since (by another process) is never undone in its place.
	pub fn undo_batch(&mut self, expected: &str) -> Result<Option<BatchReport>, ExecError> {
		self.undo_newest(Some(expected))
	}

	fn undo_newest(&mut self, expected: Option<&str>) -> Result<Option<BatchReport>, ExecError> {
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
						crate::sanitize(b.id.as_ref())
					)
				} else {
					"it was interrupted and must be rolled back first (`tpfs recover`)".into()
				},
			});
		}
		let Some(batch) = batches.into_iter().rev().find(|b| b.status == BatchStatus::Committed) else {
			return Ok(None);
		};
		if let Some(expected) = expected.filter(|e| *e != batch.id) {
			return Err(ExecError::Undo {
				batch: expected.to_owned(),
				error: format!(
					"it is no longer the newest batch to undo (that is now {}); nothing was changed",
					crate::sanitize(batch.id.as_ref())
				),
			});
		}

		if batch.damaged {
			return Err(ExecError::Undo {
				batch: batch.id,
				error: "its journal records are damaged; undo it by hand from `tpfs log`".into(),
			});
		}
		if batch.purged {
			return Err(ExecError::Undo { batch: batch.id, error: PURGED.into() });
		}
		let fs = self.fs(batch.steps.iter().flat_map(|st| [&st.inverse, &st.forward]));
		let mut reverted = 0;
		for step in batch.steps.iter().rev().filter(|s| s.done && !s.undone) {
			let fail = |error: String| ExecError::Undo { batch: batch.id.clone(), error };
			if self.cancelled() {
				return Err(fail(format!(
					"stopped on request after {reverted} step(s); they stay reverted, and undo again finishes the batch"
				)));
			}
			self.check_journaled(&step.inverse).map_err(fail)?;
			self.copy_unchanged(step, &batch.id, "undo").map_err(fail)?;
			self.replay(&fs, &batch.id, "undo", step).map_err(|e| fail(clean(format!("{}: {e}", step.inverse))))?;
			self.journal.write(&Entry::UndoStep { batch: batch.id.clone(), index: step.index })?;
			reverted += 1;
			if let Some(progress) = &mut self.progress {
				progress(reverted);
			}
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

	/// A copy changed since its batch finished is the user's now: undo and recovery keep it. Not
	/// once an earlier attempt (`what`: undo or recover) published its trash copy: what is left is
	/// that attempt's half-removed original, and `replay` checks it is all in the trash copy.
	fn copy_unchanged(&self, step: &crate::journal::Step, batch: &str, what: &str) -> Result<(), String> {
		if let (true, Op::Copy { to, .. }) = (step.undo_published, &step.forward)
			&& let Some(to) = normalize(to)
		{
			let label = format!("{batch}-{what}");
			let slot = trash_slot(&self.trash, &label, step.index, &to);
			if exists(&slot) && !exists(&part_path(&slot, &tag(&label, step.index))) {
				return Ok(());
			}
		}
		if let (Op::Copy { to, .. }, Some(check)) = (&step.forward, &step.check)
			&& let Some(to) = normalize(to)
			&& exists(&to)
			&& fingerprint(&to).as_ref() != Some(check)
			&& fingerprint_as(&to, true).as_ref() != Some(check)
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
					// An earlier attempt may have published its copy (the part is gone, renamed into
					// place) and stopped while removing the rest: only if everything left at the old
					// place is in the copy is it a duplicate. Otherwise the new place is someone else's.
					if !duplicated_in(&n(from), &n(to)) {
						return Err(both_exist(&n(from), &n(to)));
					}
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
					if !duplicated_in(&n(path), &slot) {
						return Err(both_exist(&n(path), &slot));
					}
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
		if let Op::Mkdir { path } = &step.inverse {
			return restore_dir(fs, self.journal, batch, step, &n(path), &t);
		}
		if already_applied(&step.inverse) {
			return Ok(());
		}
		let journal = &mut self.journal;
		let mut publish = || journal.write(&Entry::UndoPublished { batch: batch.to_owned(), index: step.index });
		apply(&step.inverse, fs, &self.trash, &label, step.index, None, &mut publish)
	}

	/// A journaled operation replayed by undo or recovery must stay where tpfs may act: below the
	/// validator's roots without symlinks, or in a trash. A forged or stale journal entry pointing
	/// through a planted symlink is refused instead of followed.
	fn check_journaled(&self, op: &Op) -> Result<(), String> {
		if let Op::Rename { new_name, .. } = op {
			let mut parts = Path::new(new_name).components();
			if !matches!((parts.next(), parts.next()), (Some(std::path::Component::Normal(_)), None)) {
				return Err(format!("journaled new name is not a plain name: {}", crate::sanitize(new_name.as_ref())));
			}
		}
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
				let id = crate::sanitize(b.id.as_ref());
				report.errors.push(format!(
					"batch {id} was interrupted and newer batches were applied after it, so it is not rolled back automatically: see `tpfs log {id}`, then `tpfs recover --abandon {id}` to accept the disk as it is"
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

	/// Give up on an interrupted (or damaged) batch without rolling it back: record it as abandoned, leaving the
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

	fn recover_batches(&mut self, batches: Vec<crate::journal::BatchView>) -> Result<RecoveryReport, ExecError> {
		let mut report = RecoveryReport::default();
		let fs = self.fs(batches.iter().flat_map(|b| &b.steps).flat_map(|st| [&st.inverse, &st.forward]));
		// Newest first: an older batch's inverses assume the later ones were undone already.
		for batch in batches.into_iter().rev() {
			if batch.damaged {
				report
					.errors
					.push(format!(
						"batch {id}: journal records are damaged; not rolled back automatically: see `tpfs log {id}`, then `tpfs recover --abandon {id}` to accept the disk as it is",
						id = crate::sanitize(batch.id.as_ref())
					));
				continue;
			}
			let mut errors = Vec::new();
			// Each step rolled back is recorded (as undo does), so an interrupted recovery resumes
			// after it instead of guessing from the disk, where two steps may share a path.
			for step in batch.steps.iter().rev().filter(|s| !s.undone) {
				if step.done {
					let result = self
						.check_journaled(&step.inverse)
						.and_then(|()| self.copy_unchanged(step, &batch.id, "recover"))
						.map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))
						.and_then(|()| self.replay(&fs, &batch.id, "recover", step));
					match result {
						Ok(()) => {
							self.journal.write(&Entry::UndoStep { batch: batch.id.clone(), index: step.index })?
						}
						Err(e) => {
							// Older inverses assume this one ran: stop here, as undo does.
							errors.push(clean(format!("{}: {e}", step.inverse)));
							break;
						}
					}
					continue;
				}
				if step.failed {
					// Its own call failed and nothing but its partial copy can be left (a crash between
					// the record and the clean-up): drop that, and leave everything else alone.
					let part = cleanup(&step.forward, &step.inverse, &tag(&batch.id, step.index), false)
						.into_iter()
						.filter(|f| matches!(f, Fix::RemovePart(_)));
					for fix in part {
						if let Err(e) = apply_fix(&fix, &fs, &self.trash, &format!("{}-recover", batch.id), step.index)
						{
							errors.push(clean(format!("{fix}: {e}")));
						}
					}
					continue;
				}
				let mut clean_step = true;
				let recover_tag = tag(&format!("{}-recover", batch.id), step.index);
				for fix in cleanup(&step.forward, &step.inverse, &tag(&batch.id, step.index), step.published) {
					// A removed folder comes back with its own mode, through a private name.
					if let Fix::Op(Op::Mkdir { path }) = &fix
						&& matches!(step.forward, Op::Rmdir { .. })
					{
						let path = normalize(path).unwrap_or_else(|| path.clone());
						let result = self
							.validator
							.check_path(&path)
							.map(|_| ())
							.map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))
							.and_then(|()| restore_dir(&fs, self.journal, &batch.id, step, &path, &recover_tag));
						if let Err(e) = result {
							errors.push(clean(format!("{fix}: {e}")));
							clean_step = false;
						}
						continue;
					}
					let result = match &fix {
						Fix::Op(op) => {
							self.check_journaled(op).map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))
						}
						_ => Ok(()),
					}
					.and_then(|()| apply_fix(&fix, &fs, &self.trash, &format!("{}-recover", batch.id), step.index));
					if let Err(e) = result {
						errors.push(clean(format!("{fix}: {e}")));
						clean_step = false;
					}
				}
				// A folder an earlier recovery started to restore (journaled): finish it.
				if clean_step
					&& step.undo_published
					&& let Op::Rmdir { path } = &step.forward
					&& let Some(path) = normalize(path)
					&& exists(&path)
					&& let Err(e) = restore_dir(&fs, self.journal, &batch.id, step, &path, &recover_tag)
				{
					errors.push(clean(format!("{}: {e}", step.inverse)));
					clean_step = false;
				}
				if !clean_step {
					break; // older steps wait until this one is cleaned up
				}
				self.journal.write(&Entry::UndoStep { batch: batch.id.clone(), index: step.index })?;
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

	/// Confined file operations for the roots, and for the per-file-system trash folders next to
	/// them (outside the roots, so they get anchors of their own).
	fn fs<'o>(&self, ops: impl IntoIterator<Item = &'o Op>) -> Fs {
		// The trash each item would actually use (one per file system), anchored and checked on its
		// handle — also when it lies inside a root: the deepest anchor wins. Only a trash step may
		// create one; for other paths an existing one is anchored (undo moves out of it).
		let mut trashes: Vec<PathBuf> = Vec::new();
		for op in ops {
			// Trash steps use it; so may the clean-up of a copy or move that fails half way (only on
			// the target's volume: nothing is ever trashed on a copy's source volume).
			let target = match op {
				Op::Copy { to, .. } | Op::Move { to, .. } => Some(to.as_path()),
				_ => None,
			};
			for p in op.paths() {
				let create = matches!(op, Op::Trash { .. }) || Some(p) == target;
				// A target that does not exist yet lives on the file system of its nearest existing
				// folder: that trash is used too (clean-up of a cross-device move, its undo).
				let Some(p) = p.ancestors().find(|a| fs::symlink_metadata(a).is_ok()) else { continue };
				if let Some(t) = local_trash(&self.trash, p, create)
					&& !trashes.contains(&t)
				{
					trashes.push(t);
				}
			}
		}
		Fs::confined_with_private(self.validator.roots(), &trashes)
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
pub(crate) fn absolute_real(path: &Path) -> PathBuf {
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
pub(crate) fn in_local_trash(path: &Path) -> bool {
	#[cfg(any(target_os = "linux", target_vendor = "apple"))]
	{
		use std::os::unix::fs::{MetadataExt, PermissionsExt};
		let uid = rustix::process::geteuid().as_raw();
		let name = format!(".tpfs-trash-{uid}");
		path.ancestors().any(|a| {
			a.file_name().is_some_and(|n| n == name.as_str())
				&& fs::symlink_metadata(a)
					.is_ok_and(|m| m.is_dir() && m.uid() == uid && m.permissions().mode() & 0o077 == 0)
		})
	}
	#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
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
	fingerprint_as(path, false)
}

/// `legacy`: as versions up to 0.7.4 computed it, with a symlink's own time and without its
/// target (still accepted for the copies they journaled).
fn fingerprint_as(path: &Path, legacy: bool) -> Option<String> {
	use sha2::{Digest, Sha256};
	fn walk(path: &Path, rel: &Path, h: &mut Sha256, budget: &mut usize, legacy: bool) -> Option<()> {
		*budget = budget.checked_sub(1)?;
		let m = fs::symlink_metadata(path).ok()?;
		// Folder times are left out: adding and removing entries (by later steps and their undo)
		// changes them without changing anything the user made. So is a symlink's own time: a move
		// to another file system and back (a later step and its undo) makes the link anew. What a
		// link is, is where it points.
		let link = m.file_type().is_symlink() && !legacy;
		let mtime = if m.is_dir() || link {
			0
		} else {
			m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())
		};
		h.update(format!("{}\0{}\0{}\0{mtime}\n", rel.display(), m.is_dir(), if m.is_dir() { 0 } else { m.len() }));
		if link {
			h.update(fs::read_link(path).ok()?.as_os_str().as_encoded_bytes());
			h.update(b"\n");
		}
		if m.is_dir() {
			let mut names: Vec<_> = fs::read_dir(path).ok()?.filter_map(|e| e.ok().map(|e| e.file_name())).collect();
			names.sort();
			for name in names {
				walk(&path.join(&name), &rel.join(&name), h, budget, legacy)?;
			}
		}
		Some(())
	}
	let mut h = Sha256::new();
	// Very large trees are not fingerprinted (undo then trashes them, which is still undoable).
	let mut budget = 100_000;
	walk(path, Path::new(""), &mut h, &mut budget, legacy)?;
	Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether everything in `rest` (a file, or a folder tree) is also in `copy` at the same relative
/// place, with the same kind, size and modification time (copies keep the time; 2 s tolerance for
/// FAT): then `rest` is what is left of an original after its copy was published.
fn duplicated_in(rest: &Path, copy: &Path) -> bool {
	fn same(a: &fs::Metadata, b: &fs::Metadata) -> bool {
		let t = |m: &fs::Metadata| m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
		let close = match (t(a), t(b)) {
			(Some(x), Some(y)) => x.abs_diff(y) <= std::time::Duration::from_secs(2),
			_ => false,
		};
		// A symlink is its target (compared below): a copy of one is a new link with a new time.
		a.file_type() == b.file_type() && (a.is_dir() || a.file_type().is_symlink() || (a.len() == b.len() && close))
	}
	let mut budget = 1_000_000usize;
	let mut stack = vec![(rest.to_path_buf(), copy.to_path_buf())];
	while let Some((r, c)) = stack.pop() {
		let (Ok(rm), Ok(cm)) = (fs::symlink_metadata(&r), fs::symlink_metadata(&c)) else { return false };
		if !same(&rm, &cm) {
			return false;
		}
		if rm.file_type().is_symlink() && fs::read_link(&r).ok() != fs::read_link(&c).ok() {
			return false;
		}
		if rm.is_dir() {
			let Ok(rd) = fs::read_dir(&r) else { return false };
			for e in rd {
				let Ok(e) = e else { return false };
				budget = match budget.checked_sub(1) {
					Some(b) => b,
					None => return false,
				};
				stack.push((e.path(), c.join(e.file_name())));
			}
		}
	}
	true
}

fn both_exist(old: &Path, new: &Path) -> io::Error {
	io::Error::new(
		io::ErrorKind::AlreadyExists,
		format!(
			"{} and {} both exist and differ; an earlier attempt did not finish here, or someone else put a file there. \
			 Nothing was changed: check both, keep what you need, and run it again",
			crate::sanitize(old.as_os_str()),
			crate::sanitize(new.as_os_str())
		),
	)
}

/// Bring back a folder an `rmdir` removed, with its recorded mode. It is made owner-only at a
/// private name, journaled, then renamed into place without replacing anything: a folder at the
/// name is ours only with the record and no private name left (the rename happened), so a rerun
/// after a crash at any point finishes the job, and a folder someone else made is never taken over.
fn restore_dir(
	fs: &Fs,
	journal: &mut Journal,
	batch: &str,
	step: &crate::journal::Step,
	path: &Path,
	tag: &str,
) -> io::Result<()> {
	let part = part_path(path, tag);
	let something_there = || {
		io::Error::new(
			io::ErrorKind::AlreadyExists,
			"something is there again, not made by tpfs; it is left as it is. Move or remove it yourself, \
			 then run undo again",
		)
	};
	if exists(&part) && !step.undo_published {
		remove_part(fs, &part)?; // made, but not journaled: start again
	}
	if !exists(&part) {
		if exists(path) {
			return if step.undo_published || same_empty_folder(path, step.mode) {
				finish_dir(fs, path, step.mode)
			} else {
				Err(something_there())
			};
		}
		// Private while it is made and not yet journaled.
		fs.mkdir_mode(&part, 0o700)?;
		crash_point("made")?;
		sync_parent(&part);
		journal.write(&Entry::UndoPublished { batch: batch.to_owned(), index: step.index })?;
		crash_point("journaled")?;
	}
	// Ours from here on: its mode is set while it still has the owner's bits to be opened (an
	// unrecorded part is always 0o700, so it can be removed), and checked first on a rerun.
	finish_dir(fs, &part, step.mode)?;
	crash_point("moded")?;
	match fs.rename_noreplace(&part, path) {
		Err(_) if exists(path) => return Err(something_there()),
		r => r?,
	}
	crash_point("renamed")?;
	sync_parent(path);
	finish_dir(fs, path, step.mode)
}

#[cfg(test)]
thread_local! {
	/// Where the tests stop a folder restore, as a crash would.
	pub(crate) static CRASH_AT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}

pub(crate) fn crash_point(_at: &'static str) -> io::Result<()> {
	crate::crash::point(_at);
	#[cfg(test)]
	if CRASH_AT.with(|c| c.get() == Some(_at)) {
		return Err(io::Error::other(format!("simulated crash after: {_at}")));
	}
	Ok(())
}

/// An empty folder with exactly the recorded mode: as good as restored (Unix only; without modes
/// nothing tells it apart from someone else's).
fn same_empty_folder(path: &Path, mode: Option<u32>) -> bool {
	let Ok(m) = fs::symlink_metadata(path) else { return false };
	#[cfg(unix)]
	let same = {
		use std::os::unix::fs::PermissionsExt;
		mode == Some(m.permissions().mode() & 0o7777)
	};
	#[cfg(not(unix))]
	let same = {
		let _ = mode;
		false
	};
	m.is_dir() && same && fs::read_dir(path).is_ok_and(|mut rd| rd.next().is_none())
}

/// Give a restored folder its recorded mode, unless it already has it.
fn finish_dir(fs: &Fs, path: &Path, mode: Option<u32>) -> io::Result<()> {
	#[cfg(unix)]
	if let Some(mode) = mode {
		use std::os::unix::fs::PermissionsExt;
		if fs::symlink_metadata(path)?.permissions().mode() & 0o7777 != mode {
			return set_mode(fs, path, mode);
		}
	}
	let _ = (fs, path, mode);
	Ok(())
}

/// Give a folder tpfs recreated the permissions it had before its `rmdir` (Unix: elsewhere no
/// mode is recorded).
#[cfg(unix)]
fn set_mode(fs: &Fs, path: &Path, mode: u32) -> io::Result<()> {
	use std::os::unix::fs::PermissionsExt;
	crate::fsops::set_dir_mode_at(fs, path, fs::Permissions::from_mode(mode))?;
	// The folder itself was synced through the handle that changed its mode.
	sync_parent(path);
	Ok(())
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
			// <trash>/<batch>/<index>/<name>: the trash folder exists; the two levels below are made
			// through the confined calls (a local trash is anchored like a root, never followed).
			let batch_dir = parent.parent().ok_or_else(|| io::Error::other("trash slot without a batch folder"))?;
			if let Some(base) = batch_dir.parent() {
				if base == trash {
					if !exists(base) {
						crate::crash::point("trash.create");
						fs::create_dir_all(base)?; // the private trash in the state folder
					}
				} else if !fs.anchors_exactly(base) {
					// A per-file-system trash is only ever used through its checked anchor.
					return Err(io::Error::new(
						io::ErrorKind::PermissionDenied,
						format!("refused: the trash {} was not opened safely", crate::sanitize(base.as_os_str())),
					));
				}
			}
			for d in [batch_dir, parent] {
				match fs.mkdir(d) {
					Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
					_ => {}
				}
			}
			// Make the new slot folders durable: trash/<batch> in trash, <index> in trash/<batch>.
			if let Some(batch_dir) = parent.parent() {
				sync_parent(batch_dir);
			}
			sync_parent(parent);
			let part = part_path(&slot, &tag);
			let moved = fs.move_any(&path, &slot, &part, publish);
			// A clean-up trash (undo, recovery) takes away a copy tpfs made. One of a read-only
			// folder has that folder's mode, and a folder its owner may not write to cannot be
			// renamed into another folder: without this, such a copy could never be undone. The
			// copy is ours, so it is opened and moved; a folder the user asked to trash is not.
			if planned.is_none()
				&& moved.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied)
				&& fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir())
				&& make_dir_writable(fs, &path).is_ok()
			{
				return fs.move_any(&path, &slot, &part, publish);
			}
			moved
		}
		Op::Mkdir { path } => {
			// Made at a private name, journaled, then renamed into place without replacing
			// anything: a folder at the name is ours exactly when the step is published and its
			// private name is gone, so recovery never mistakes ours for someone else's or the other
			// way round (see `cleanup`).
			let path = n(path)?;
			let part = part_path(&path, &tag);
			fs.mkdir(&part)?;
			// The entry is durable before the record that calls it ours.
			sync_parent(&part);
			publish()?;
			crash_point("mkdir-published")?;
			fs.rename_noreplace(&part, &path)?;
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
		Fix::Op(op) => {
			// An earlier recovery that stopped while copying the item back (across file systems)
			// left its partial copy under this very name: dropped first, as undo does, or the copy
			// could never be made again and the batch would stay interrupted for good.
			if let Op::Move { to, .. } = op
				&& let Some(to) = normalize(to)
			{
				remove_part(fs, &part_path(&to, &tag(batch, index)))?;
			}
			apply(op, fs, trash, batch, index, None, &mut || Ok(()))
		}
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
			let part = part_path(&n(path), tag);
			if exists(&part) {
				fixes.push(Fix::RemovePart(part));
			} else if published && exists(&n(path)) {
				// Renamed into place by this step: ours. Without that record it is someone else's.
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
	let base = local_trash(trash, path, true).unwrap_or_else(|| trash.to_path_buf());
	base.join(batch).join(index.to_string()).join(path.file_name().unwrap_or_else(|| "root".as_ref()))
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
pub(crate) fn local_trash(trash: &Path, path: &Path, create: bool) -> Option<PathBuf> {
	use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

	let dev = fs::symlink_metadata(path).ok()?.dev();
	let trash_dev = trash.ancestors().find_map(|a| fs::metadata(a).ok())?.dev();
	if trash_dev == dev {
		return None;
	}

	// The mount point: the highest ancestor (or the path itself) still on the same device.
	let mut mount = path;
	for a in path.ancestors().skip(1) {
		match fs::metadata(a) {
			Ok(m) if m.dev() == dev => mount = a,
			_ => break,
		}
	}

	let uid = rustix::process::geteuid().as_raw();
	let dir = mount.join(format!(".tpfs-trash-{uid}"));
	if create {
		let _ = fs::DirBuilder::new().mode(0o700).create(&dir);
	}

	// Never use a trash someone else prepared (a symlink, another owner, readable by others).
	let m = fs::symlink_metadata(&dir).ok()?;
	(m.is_dir() && m.uid() == uid && m.permissions().mode() & 0o077 == 0).then_some(dir)
}

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
pub(crate) fn local_trash(_: &Path, _: &Path, _: bool) -> Option<PathBuf> {
	None
}

pub(crate) fn now_secs() -> u64 {
	SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn new_batch_id() -> String {
	let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
	format!("{nanos:x}-{:x}", std::process::id())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{Plan, Validator};

	/// A copy never enters another disk mounted inside the folder: it stops there and leaves
	/// nothing behind (a move falling back to a copy would otherwise empty that disk).
	#[test]
	fn a_copy_stops_at_a_disk_mounted_inside() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(tmp.path()).unwrap();
		let root = base.join("root");
		let inner = root.join("src").join("inner");
		fs::create_dir_all(&inner).unwrap();
		fs::write(root.join("src").join("a"), "A").unwrap();
		fs::write(inner.join("file"), "F").unwrap();
		crate::validate::FAKE_MOUNT.with(|f| *f.borrow_mut() = Some(inner.clone()));
		let v = Validator::new([&root]).unwrap();
		let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
		let dst = root.join("dst");
		let err = Executor::new(&v, &mut j, base.join("state").join("trash"))
			.execute(&Plan::new(vec![Op::Copy { from: root.join("src"), to: dst.clone() }], ""), Source::Human)
			.unwrap_err();
		assert!(err.to_string().contains("mount point"), "{err}");
		assert!(!dst.exists());
		assert!(fs::read_dir(&root).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".tpfs-")));
		assert_eq!(fs::read_to_string(inner.join("file")).unwrap(), "F");
	}

	/// An undo of a copy whose trash went through a copy (another volume) and stopped while
	/// removing the original resumes: the rest is not mistaken for a change by the user.
	#[test]
	fn an_interrupted_undo_of_a_copy_resumes() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(tmp.path()).unwrap();
		let root = base.join("root");
		fs::create_dir_all(root.join("src")).unwrap();
		fs::write(root.join("src").join("a"), "A").unwrap();
		fs::write(root.join("src").join("b"), "B").unwrap();
		let v = Validator::new([&root]).unwrap();
		let trash = base.join("state").join("trash");
		let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
		let dst = root.join("dst");
		let id = Executor::new(&v, &mut j, &trash)
			.execute(&Plan::new(vec![Op::Copy { from: root.join("src"), to: dst.clone() }], ""), Source::Human)
			.unwrap()
			.id;
		// The interrupted undo: a full copy published in its trash slot, the original half removed.
		let slot = trash_slot(&trash, &format!("{id}-undo"), 0, &dst);
		fs::create_dir_all(&slot).unwrap();
		fs::write(slot.join("a"), "A").unwrap();
		fs::write(slot.join("b"), "B").unwrap();
		fs::remove_file(dst.join("b")).unwrap();
		j.append(&Entry::UndoPublished { batch: id.clone(), index: 0 }).unwrap();
		let done = Executor::new(&v, &mut j, &trash).undo_last().unwrap().unwrap();
		assert_eq!(done.id, id);
		assert!(!dst.exists());
		assert_eq!(fs::read_to_string(slot.join("b")).unwrap(), "B");
	}

	/// A resumed undo of a copy (its trash copy published) whose half-removed rest the user
	/// changed meanwhile stops and keeps both: the rest is trashed only if all of it is in the copy.
	#[test]
	fn a_resumed_undo_of_a_copy_keeps_what_the_user_changed() {
		for edit in ["changed", "added"] {
			let tmp = tempfile::TempDir::new().unwrap();
			let base = crate::path::canonicalize(tmp.path()).unwrap();
			let root = base.join("root");
			fs::create_dir_all(root.join("src")).unwrap();
			fs::write(root.join("src").join("a"), "A").unwrap();
			fs::write(root.join("src").join("b"), "B").unwrap();
			let v = Validator::new([&root]).unwrap();
			let trash = base.join("state").join("trash");
			let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
			let dst = root.join("dst");
			let id = Executor::new(&v, &mut j, &trash)
				.execute(&Plan::new(vec![Op::Copy { from: root.join("src"), to: dst.clone() }], ""), Source::Human)
				.unwrap()
				.id;
			let slot = trash_slot(&trash, &format!("{id}-undo"), 0, &dst);
			fs::create_dir_all(&slot).unwrap();
			fs::write(slot.join("a"), "A").unwrap();
			fs::write(slot.join("b"), "B").unwrap();
			fs::remove_file(dst.join("b")).unwrap();
			j.append(&Entry::UndoPublished { batch: id.clone(), index: 0 }).unwrap();
			if edit == "changed" {
				fs::write(dst.join("a"), "edited after the interruption").unwrap();
			} else {
				fs::write(dst.join("new"), "N").unwrap();
			}
			assert!(Executor::new(&v, &mut j, &trash).undo_last().is_err(), "{edit}");
			assert!(dst.exists(), "{edit}");
			if edit == "changed" {
				assert_eq!(fs::read_to_string(dst.join("a")).unwrap(), "edited after the interruption");
			} else {
				assert_eq!(fs::read_to_string(dst.join("new")).unwrap(), "N");
			}
			assert_eq!(fs::read_to_string(slot.join("b")).unwrap(), "B");
		}
	}

	/// Progress counts each step; a cancel request stops before the next step and rolls the batch
	/// back (all or nothing), and a cancelled undo keeps what it reverted and finishes when run again.
	#[test]
	fn progress_and_cancel() {
		use std::sync::{
			Arc,
			atomic::{AtomicBool, Ordering},
		};
		let tmp = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(tmp.path()).unwrap();
		let root = base.join("root");
		fs::create_dir_all(&root).unwrap();
		let v = Validator::new([&root]).unwrap();
		let trash = base.join("state").join("trash");
		let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
		let plan = Plan::new((0..3).map(|i| Op::Mkdir { path: root.join(format!("d{i}")) }).collect(), "");

		let mut seen = Vec::new();
		Executor::new(&v, &mut j, &trash).on_step_done(|n| seen.push(n)).execute(&plan, Source::Human).unwrap();
		assert_eq!(seen, [1, 2, 3]);
		Executor::new(&v, &mut j, &trash).undo_last().unwrap();

		let stop = Arc::new(AtomicBool::new(false));
		let flag = stop.clone();
		let err = Executor::new(&v, &mut j, &trash)
			.cancel_on(stop.clone())
			.on_step_done(move |n| flag.store(n == 1, Ordering::Relaxed))
			.execute(&plan, Source::Human)
			.unwrap_err();
		assert!(
			matches!(err, ExecError::Cancelled { step: 1, ref rollback_errors } if rollback_errors.is_empty()),
			"{err}"
		);
		assert!((0..3).all(|i| !root.join(format!("d{i}")).exists()));
		assert_eq!(j.batches().unwrap().last().unwrap().status, BatchStatus::Aborted);

		Executor::new(&v, &mut j, &trash).execute(&plan, Source::Human).unwrap();
		let flag = stop.clone();
		stop.store(false, Ordering::Relaxed);
		let err = Executor::new(&v, &mut j, &trash)
			.cancel_on(stop.clone())
			.on_step_done(move |n| flag.store(n == 1, Ordering::Relaxed))
			.undo_last()
			.unwrap_err();
		assert!(err.to_string().contains("stopped on request after 1 step"), "{err}");
		assert_eq!((0..3).filter(|i| root.join(format!("d{i}")).exists()).count(), 2);
		assert_eq!(Executor::new(&v, &mut j, &trash).undo_last().unwrap().unwrap().steps, 2);
		assert!((0..3).all(|i| !root.join(format!("d{i}")).exists()));
	}

	/// What changed on disk after validation is caught by each step's own re-check, even where no
	/// file system call would refuse it: a disk mounted at a later step's path stops the batch,
	/// and the earlier steps are rolled back.
	#[test]
	fn each_step_is_checked_again_just_before_it_runs() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(tmp.path()).unwrap();
		let root = base.join("root");
		let m = root.join("m");
		fs::create_dir_all(&m).unwrap();
		let v = Validator::new([&root]).unwrap();
		let trash = base.join("state").join("trash");
		let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
		let mounted = m.clone();
		let err = Executor::new(&v, &mut j, &trash)
			.hook(move |i, p| {
				if i == 0 && p == Phase::AfterApply {
					crate::validate::FAKE_MOUNT.with(|f| *f.borrow_mut() = Some(mounted.clone()));
				}
			})
			.execute(
				&Plan::new(vec![Op::Mkdir { path: root.join("a") }, Op::Trash { path: m.clone() }], ""),
				Source::Human,
			)
			.unwrap_err();
		crate::validate::FAKE_MOUNT.with(|f| *f.borrow_mut() = None);
		assert!(matches!(&err, ExecError::Failed { step: 1, .. }) && err.to_string().contains("mount point"), "{err}");
		assert!(m.is_dir() && !root.join("a").exists());
	}

	/// A resumed undo of a move whose copy back was published: what is left at the moved-to place
	/// goes to the trash only if all of it is in the copy; a file there of another size is not.
	#[test]
	fn a_resumed_undo_of_a_move_keeps_a_different_leftover() {
		for same in [true, false] {
			let tmp = tempfile::TempDir::new().unwrap();
			let base = crate::path::canonicalize(tmp.path()).unwrap();
			let root = base.join("root");
			fs::create_dir_all(root.join("dir")).unwrap();
			fs::write(root.join("dir").join("f"), "F").unwrap();
			let v = Validator::new([&root]).unwrap();
			let trash = base.join("state").join("trash");
			let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
			let away = root.join("away");
			let id = Executor::new(&v, &mut j, &trash)
				.execute(&Plan::new(vec![Op::Move { from: root.join("dir"), to: away.clone() }], ""), Source::Human)
				.unwrap()
				.id;
			// The interrupted undo: its copy back published at `dir`, `away` not yet removed.
			fs::create_dir(root.join("dir")).unwrap();
			fs::write(root.join("dir").join("f"), "F").unwrap();
			let m = fs::metadata(away.join("f")).unwrap();
			fs::File::options()
				.write(true)
				.open(root.join("dir").join("f"))
				.unwrap()
				.set_modified(m.modified().unwrap())
				.unwrap();
			if !same {
				fs::write(away.join("f"), "a different, longer content").unwrap();
			}
			j.append(&Entry::UndoPublished { batch: id.clone(), index: 0 }).unwrap();
			let result = Executor::new(&v, &mut j, &trash).undo_last();
			if same {
				result.unwrap().unwrap();
				assert!(!away.exists());
			} else {
				let err = result.unwrap_err();
				assert!(err.to_string().contains("exist"), "{err}");
				assert_eq!(fs::read_to_string(away.join("f")).unwrap(), "a different, longer content");
			}
			assert_eq!(fs::read_to_string(root.join("dir").join("f")).unwrap(), "F");
		}
	}

	/// An undo of a copy cut short before its trash copy was published still notices a change
	/// the user made to the copy since.
	#[test]
	fn an_unpublished_undo_of_a_copy_still_keeps_a_changed_copy() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(tmp.path()).unwrap();
		let root = base.join("root");
		fs::create_dir_all(root.join("src")).unwrap();
		fs::write(root.join("src").join("a"), "A").unwrap();
		let v = Validator::new([&root]).unwrap();
		let trash = base.join("state").join("trash");
		let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
		let dst = root.join("dst");
		let id = Executor::new(&v, &mut j, &trash)
			.execute(&Plan::new(vec![Op::Copy { from: root.join("src"), to: dst.clone() }], ""), Source::Human)
			.unwrap()
			.id;
		let label = format!("{id}-undo");
		let slot = trash_slot(&trash, &label, 0, &dst);
		fs::create_dir_all(part_path(&slot, &tag(&label, 0))).unwrap();
		j.append(&Entry::UndoPublished { batch: id.clone(), index: 0 }).unwrap();
		fs::write(dst.join("a"), "edited").unwrap();
		fs::write(dst.join("new"), "N").unwrap();
		let err = Executor::new(&v, &mut j, &trash).undo_last().unwrap_err();
		assert!(err.to_string().contains("changed after it was copied"), "{err}");
		assert_eq!(fs::read_to_string(dst.join("a")).unwrap(), "edited");
	}

	fn rmdir_batch(base: &Path) -> (Validator, Journal, PathBuf, PathBuf, String) {
		let root = base.join("root");
		let d = root.join("shared");
		fs::create_dir_all(&d).unwrap();
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
		}
		let v = Validator::new([&root]).unwrap();
		let trash = base.join("state").join("trash");
		let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
		let id = Executor::new(&v, &mut j, &trash)
			.execute(&Plan::new(vec![Op::Rmdir { path: d.clone() }], ""), Source::Human)
			.unwrap()
			.id;
		(v, j, trash, d, id)
	}

	/// An undo that made its private folder and journaled it, but lost the rename to a folder
	/// someone else made at the name, never takes that folder over, however often it is rerun.
	#[test]
	fn a_restored_folder_never_replaces_one_made_meanwhile() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(tmp.path()).unwrap();
		let (v, mut j, trash, d, id) = rmdir_batch(&base);
		let part = part_path(&d, &tag(&format!("{id}-undo"), 0));
		fs::create_dir(&part).unwrap();
		j.append(&Entry::UndoPublished { batch: id.clone(), index: 0 }).unwrap();
		fs::create_dir(&d).unwrap(); // someone else's, private and empty
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
		}
		for _ in 0..2 {
			let err = Executor::new(&v, &mut j, &trash).undo_last().unwrap_err();
			assert!(err.to_string().contains("something is there again"), "{err}");
		}
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
		}
		// Once the user moves theirs away, the undo finishes with the original mode.
		fs::remove_dir(&d).unwrap();
		Executor::new(&v, &mut j, &trash).undo_last().unwrap().unwrap();
		assert!(d.is_dir() && !part.exists());
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o755);
		}
	}

	/// A symlink made anew with the same target (a move to another file system and back) is the
	/// same link: the copy it is in still counts as unchanged. Its own time is not compared.
	#[cfg(unix)]
	#[test]
	fn a_copy_fingerprint_knows_a_symlink_by_its_target() {
		let tmp = tempfile::TempDir::new().unwrap();
		let dir = tmp.path().join("copy");
		fs::create_dir(&dir).unwrap();
		fs::write(dir.join("file"), "F").unwrap();
		let link = dir.join("link");
		std::os::unix::fs::symlink("file", &link).unwrap();
		let before = fingerprint(&dir).unwrap();
		let legacy = fingerprint_as(&dir, true).unwrap();

		std::thread::sleep(std::time::Duration::from_millis(20));
		fs::remove_file(&link).unwrap();
		std::os::unix::fs::symlink("file", &link).unwrap();
		assert_eq!(fingerprint(&dir).unwrap(), before);
		assert_ne!(fingerprint_as(&dir, true).unwrap(), legacy, "the old fingerprint took this for a change");

		fs::remove_file(&link).unwrap();
		std::os::unix::fs::symlink("elsewhere", &link).unwrap();
		assert_ne!(fingerprint(&dir).unwrap(), before, "another target is a change");
	}

	/// A crash at every point of a folder restore, in undo and in recovery: each rerun (twice)
	/// finishes it with the recorded mode, and nothing is left at a private name.
	#[test]
	fn a_crash_anywhere_in_a_folder_restore_is_finished() {
		for at in ["made", "journaled", "moded", "renamed"] {
			// Undo.
			let tmp = tempfile::TempDir::new().unwrap();
			let base = crate::path::canonicalize(tmp.path()).unwrap();
			let (v, mut j, trash, d, _) = rmdir_batch(&base);
			CRASH_AT.with(|c| c.set(Some(at)));
			let err = Executor::new(&v, &mut j, &trash).undo_last().unwrap_err();
			CRASH_AT.with(|c| c.set(None));
			assert!(err.to_string().contains("simulated crash"), "{at}: {err}");
			#[cfg(unix)]
			if let Ok(m) = fs::symlink_metadata(&d) {
				use std::os::unix::fs::PermissionsExt;
				// Never wider than the recorded mode while in place, and already that mode once
				// it is in place (it was set at the private name).
				assert_eq!(m.permissions().mode() & 0o777 & !0o755, 0, "{at}");
				if at == "renamed" {
					assert_eq!(m.permissions().mode() & 0o777, 0o755, "{at}");
				}
			}
			Executor::new(&v, &mut j, &trash).undo_last().unwrap().unwrap();
			assert!(Executor::new(&v, &mut j, &trash).undo_last().unwrap().is_none(), "{at}");
			assert!(d.is_dir(), "{at}");
			assert_no_parts(&base.join("root"));
			#[cfg(unix)]
			{
				use std::os::unix::fs::PermissionsExt;
				assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o755, "{at}");
			}

			// Recovery of a batch interrupted after its rmdir ran.
			let tmp = tempfile::TempDir::new().unwrap();
			let base = crate::path::canonicalize(tmp.path()).unwrap();
			let root = base.join("root");
			let d = root.join("shared");
			fs::create_dir_all(&root).unwrap();
			let v = Validator::new([&root]).unwrap();
			let trash = base.join("state").join("trash");
			let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
			for e in [
				Entry::Begin {
					batch: "b".into(),
					at: 0,
					source: "human".into(),
					reason: String::new(),
					digest: String::new(),
					v: 1,
				},
				Entry::Intent {
					batch: "b".into(),
					index: 0,
					forward: Op::Rmdir { path: d.clone() },
					inverse: Op::Mkdir { path: d.clone() },
				},
				Entry::Mode { batch: "b".into(), index: 0, mode: 0o755 },
			] {
				j.append(&e).unwrap();
			}
			CRASH_AT.with(|c| c.set(Some(at)));
			let report = Executor::new(&v, &mut j, &trash).recover().unwrap();
			CRASH_AT.with(|c| c.set(None));
			assert!(!report.errors.is_empty(), "{at}: {report:?}");
			let report = Executor::new(&v, &mut j, &trash).recover().unwrap();
			assert!(report.errors.is_empty(), "{at}: {report:?}");
			let again = Executor::new(&v, &mut j, &trash).recover().unwrap();
			assert!(again.errors.is_empty() && again.rolled_back.is_empty(), "{at}: {again:?}");
			assert!(d.is_dir(), "{at}");
			assert_no_parts(&root);
			#[cfg(unix)]
			{
				use std::os::unix::fs::PermissionsExt;
				assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o755, "{at}");
			}
		}
	}

	fn assert_no_parts(dir: &Path) {
		assert!(
			fs::read_dir(dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".tpfs-part-")),
			"a private name was left in {}",
			dir.display()
		);
	}

	fn journal_batch(j: &mut Journal, batch: &str, entries: Vec<Entry>) {
		j.append(&Entry::Begin {
			batch: batch.into(),
			at: 0,
			source: "human".into(),
			reason: String::new(),
			digest: String::new(),
			v: 1,
		})
		.unwrap();
		for e in entries {
			j.append(&e).unwrap();
		}
	}

	/// A folder whose recorded mode does not let its owner open it: a restore cut short after the
	/// mode was set still finishes (nothing has to open the folder again).
	#[cfg(unix)]
	#[test]
	fn a_restore_cut_short_after_a_mode_without_read_finishes() {
		use std::os::unix::fs::PermissionsExt;
		if rustix::process::geteuid().is_root() {
			eprintln!("note: run as root, so the missing read bit is not enforced here (CI runs it unprivileged)");
		}
		let tmp = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(tmp.path()).unwrap();
		let root = base.join("root");
		fs::create_dir_all(&root).unwrap();
		let d = root.join("locked");
		let v = Validator::new([&root]).unwrap();
		let trash = base.join("state").join("trash");
		let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
		let b = "18e97de4eaba61d8-1";
		journal_batch(
			&mut j,
			b,
			vec![
				Entry::Intent {
					batch: b.into(),
					index: 0,
					forward: Op::Rmdir { path: d.clone() },
					inverse: Op::Mkdir { path: d.clone() },
				},
				Entry::Mode { batch: b.into(), index: 0, mode: 0o055 },
				Entry::Done { batch: b.into(), index: 0, check: None, mode: Some(0o055) },
				Entry::Commit { batch: b.into() },
			],
		);
		CRASH_AT.with(|c| c.set(Some("moded")));
		assert!(Executor::new(&v, &mut j, &trash).undo_last().is_err());
		CRASH_AT.with(|c| c.set(None));
		Executor::new(&v, &mut j, &trash).undo_last().unwrap().unwrap();
		assert_eq!(fs::symlink_metadata(&d).unwrap().permissions().mode() & 0o777, 0o055);
		assert_no_parts(&root);
		fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
	}

	/// `rmdir x` then `mkdir x` in one batch, cut short after the new folder was made at its
	/// private name and before it was journaled: recovery drops it and restores the old folder.
	#[test]
	fn rmdir_then_mkdir_cut_short_is_rolled_back() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(tmp.path()).unwrap();
		let root = base.join("root");
		fs::create_dir_all(&root).unwrap();
		let d = root.join("x");
		let v = Validator::new([&root]).unwrap();
		let trash = base.join("state").join("trash");
		let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
		let b = "18e97de4eaba61d8-2";
		journal_batch(
			&mut j,
			b,
			vec![
				Entry::Intent {
					batch: b.into(),
					index: 0,
					forward: Op::Rmdir { path: d.clone() },
					inverse: Op::Mkdir { path: d.clone() },
				},
				Entry::Mode { batch: b.into(), index: 0, mode: 0o700 },
				Entry::Done { batch: b.into(), index: 0, check: None, mode: Some(0o700) },
				Entry::Intent {
					batch: b.into(),
					index: 1,
					forward: Op::Mkdir { path: d.clone() },
					inverse: Op::Rmdir { path: d.clone() },
				},
			],
		);
		fs::create_dir(part_path(&d, &tag(b, 1))).unwrap();
		for _ in 0..2 {
			let report = Executor::new(&v, &mut j, &trash).recover().unwrap();
			assert!(report.errors.is_empty(), "{report:?}");
		}
		assert!(d.is_dir());
		assert_no_parts(&root);
	}

	/// A `mkdir` cut short after it was journaled and before its folder was renamed into place:
	/// recovery removes the private folder and keeps a folder someone else made at the name.
	#[test]
	fn a_mkdir_cut_short_before_its_rename_is_rolled_back() {
		for someone_else in [false, true] {
			let tmp = tempfile::TempDir::new().unwrap();
			let base = crate::path::canonicalize(tmp.path()).unwrap();
			let root = base.join("root");
			fs::create_dir_all(&root).unwrap();
			let d = root.join("new");
			let v = Validator::new([&root]).unwrap();
			let trash = base.join("state").join("trash");
			let mut j = Journal::open(base.join("state").join("journal.jsonl")).unwrap();
			CRASH_AT.with(|c| c.set(Some("mkdir-published")));
			let result = Executor::new(&v, &mut j, &trash)
				.execute(&Plan::new(vec![Op::Mkdir { path: d.clone() }], ""), Source::Human);
			CRASH_AT.with(|c| c.set(None));
			assert!(result.is_err());
			assert!(!d.exists());
			assert_no_parts(&root);
			// The failure was rolled back at once; build the crashed state again by hand to
			// recover it as after a real crash: the part made and journaled, not renamed.
			let b = "18e97de4eaba61d8-3";
			journal_batch(
				&mut j,
				b,
				vec![
					Entry::Intent {
						batch: b.into(),
						index: 0,
						forward: Op::Mkdir { path: d.clone() },
						inverse: Op::Rmdir { path: d.clone() },
					},
					Entry::Published { batch: b.into(), index: 0 },
				],
			);
			fs::create_dir(part_path(&d, &tag(b, 0))).unwrap();
			if someone_else {
				fs::create_dir(&d).unwrap();
				fs::write(d.join("theirs"), "T").unwrap();
			}
			for _ in 0..2 {
				let report = Executor::new(&v, &mut j, &trash).recover().unwrap();
				assert!(report.errors.is_empty(), "{report:?}");
			}
			assert_no_parts(&root);
			assert_eq!(d.exists(), someone_else);
			if someone_else {
				assert_eq!(fs::read_to_string(d.join("theirs")).unwrap(), "T");
			}
		}
	}

	/// A private folder an undo made but never journaled (a crash in between) is dropped and the
	/// restore starts again.
	#[test]
	fn an_unjournaled_private_folder_is_made_again() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(tmp.path()).unwrap();
		let (v, mut j, trash, d, id) = rmdir_batch(&base);
		let part = part_path(&d, &tag(&format!("{id}-undo"), 0));
		fs::create_dir(&part).unwrap();
		Executor::new(&v, &mut j, &trash).undo_last().unwrap().unwrap();
		assert!(d.is_dir() && !part.exists());
	}
}
