//! Broad guarantees, checked over many generated cases: every operation's failure and undo paths,
//! random plans against the real file system, and the journal parser against random damage.

use std::{
	fs,
	path::{Path, PathBuf},
};

use tpfs_core::{BatchStatus, ExecError, Executor, Journal, Op, Plan, Source, Validator};

struct Sandbox {
	_tmp: tempfile::TempDir,
	root: PathBuf,
	state: PathBuf,
}

impl Sandbox {
	fn new() -> Self {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		let (root, state) = (base.join("root"), base.join("state"));
		fs::create_dir(&root).unwrap();
		Self { _tmp: tmp, root, state }
	}

	/// a.txt, b.txt, d/ (with d/x.txt), e/ (empty)
	fn tree(&self) {
		fs::write(self.root.join("a.txt"), "A").unwrap();
		fs::write(self.root.join("b.txt"), "B").unwrap();
		fs::create_dir(self.root.join("d")).unwrap();
		fs::write(self.root.join("d/x.txt"), "X").unwrap();
		fs::create_dir(self.root.join("e")).unwrap();
	}

	fn validator(&self) -> Validator {
		Validator::new([&self.root]).unwrap()
	}

	fn journal(&self) -> Journal {
		Journal::open(self.state.join("journal.jsonl")).unwrap()
	}

	fn trash(&self) -> PathBuf {
		self.state.join("trash")
	}

	fn snapshot(&self) -> Vec<String> {
		fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
			for e in fs::read_dir(dir).unwrap() {
				let p = e.unwrap().path();
				let rel = p.strip_prefix(base).unwrap().display().to_string();
				if p.is_dir() {
					out.push(format!("{rel}/"));
					walk(base, &p, out);
				} else {
					out.push(format!("{rel}={}", fs::read_to_string(&p).unwrap_or_default()));
				}
			}
		}
		let mut out = Vec::new();
		walk(&self.root, &self.root, &mut out);
		out.sort();
		out
	}
}

/// One operation of each kind, valid on the `tree()`.
fn one_of_each(r: &Path) -> Vec<Op> {
	vec![
		Op::Move { from: r.join("a.txt"), to: r.join("e/a.txt") },
		Op::Copy { from: r.join("d"), to: r.join("d2") },
		Op::Rename { path: r.join("b.txt"), new_name: "c.txt".into() },
		Op::Trash { path: r.join("d/x.txt") },
		Op::Mkdir { path: r.join("new") },
		Op::Rmdir { path: r.join("e") },
	]
}

#[test]
fn every_operation_is_rolled_back_when_a_later_step_fails() {
	for i in 0..6 {
		let s = Sandbox::new();
		s.tree();
		let before = s.snapshot();
		let first = one_of_each(&s.root).swap_remove(i);
		// Step 2 targets a path that someone creates right before it runs.
		let blocker = s.root.join("late.txt");
		let plan =
			Plan::new(vec![first.clone(), Op::Copy { from: s.root.join("b.txt").clone(), to: blocker.clone() }], "");
		let plan = if matches!(first, Op::Rename { .. }) {
			// b.txt is renamed by step 1; copy something else.
			Plan::new(vec![first.clone(), Op::Mkdir { path: blocker.clone() }], "")
		} else {
			plan
		};
		let v = s.validator();
		let mut j = s.journal();
		let b = blocker.clone();
		let err = Executor::new(&v, &mut j, s.trash())
			.before_apply(move |step| {
				if step == 0 {
					fs::write(&b, "someone else's").unwrap();
				}
			})
			.execute(&plan, Source::Human)
			.unwrap_err();
		match err {
			ExecError::Failed { step: 1, rollback_errors, .. } => {
				assert!(rollback_errors.is_empty(), "{first}: {rollback_errors:?}")
			}
			other => panic!("{first}: {other}"),
		}
		let mut expected = before.clone();
		expected.push("late.txt=someone else's".into());
		expected.sort();
		assert_eq!(s.snapshot(), expected, "{first} was not rolled back");
	}
}

#[test]
fn undo_stops_instead_of_clobbering_a_later_change() {
	// For each operation: what the user does afterwards that an undo must not destroy.
	for i in 0..6 {
		let s = Sandbox::new();
		s.tree();
		let op = one_of_each(&s.root).swap_remove(i);
		let mut j = s.journal();
		Executor::new(&s.validator(), &mut j, s.trash())
			.execute(&Plan::new(vec![op.clone()], ""), Source::Human)
			.unwrap();
		let r = &s.root;
		let precious = match &op {
			Op::Move { from, .. } | Op::Rename { path: from, .. } | Op::Trash { path: from } => {
				fs::write(from, "new file at the old place").unwrap();
				from.clone()
			}
			Op::Copy { to, .. } => {
				fs::write(to.join("added.txt"), "added into the copy").unwrap();
				to.join("added.txt")
			}
			Op::Mkdir { path } => {
				fs::write(path.join("inside.txt"), "put inside").unwrap();
				path.join("inside.txt")
			}
			Op::Rmdir { path } => {
				fs::write(path, "a file where the folder was").unwrap();
				path.clone()
			}
			_ => unreachable!(),
		};
		let content = fs::read_to_string(&precious).unwrap();
		let result = Executor::new(&s.validator(), &mut j, s.trash()).undo_last();
		assert!(result.is_err(), "{op}: undo went ahead");
		assert_eq!(fs::read_to_string(&precious).unwrap(), content, "{op}: the user's change was touched");
		let _ = r;
	}
}

/// A tiny deterministic generator (no extra dependencies).
struct Rng(u64);

impl Rng {
	fn next(&mut self) -> u64 {
		self.0 ^= self.0 << 13;
		self.0 ^= self.0 >> 7;
		self.0 ^= self.0 << 17;
		self.0
	}

	fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
		&items[(self.next() % items.len() as u64) as usize]
	}
}

/// Random plans over a small tree: whatever validates must execute; after undo (or a rollback)
/// the tree is exactly as before.
#[test]
fn random_plans_execute_as_validated_and_undo_completely() {
	let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
	let names = ["a.txt", "b.txt", "d", "e", "d/x.txt", "e/a.txt", "n", "n/y", "c.txt", "d2"];
	let mut executed = 0;
	for case in 0..300 {
		let s = Sandbox::new();
		s.tree();
		let before = s.snapshot();
		let p = |rng: &mut Rng| s.root.join(rng.pick(&names));
		let ops: Vec<Op> = (0..1 + rng.next() % 4)
			.map(|_| match rng.next() % 6 {
				0 => Op::Move { from: p(&mut rng), to: p(&mut rng) },
				1 => Op::Copy { from: p(&mut rng), to: p(&mut rng) },
				2 => Op::Rename { path: p(&mut rng), new_name: rng.pick(&["r1", "r2.txt"]).to_string() },
				3 => Op::Trash { path: p(&mut rng) },
				4 => Op::Mkdir { path: p(&mut rng) },
				_ => Op::Rmdir { path: p(&mut rng) },
			})
			.collect();
		let plan = Plan::new(ops, "");
		let v = s.validator();
		if !v.validate(&plan).is_empty() {
			continue;
		}
		let mut j = s.journal();
		let batch = Executor::new(&v, &mut j, s.trash()).execute(&plan, Source::Human);
		let batch = batch.unwrap_or_else(|e| panic!("case {case}: validated but failed: {e}\n{:?}", plan.preview()));
		executed += 1;
		let undone = Executor::new(&v, &mut j, s.trash()).undo_last().unwrap().unwrap();
		assert_eq!(undone.id, batch.id);
		assert_eq!(s.snapshot(), before, "case {case}: undo did not restore\n{:?}", plan.preview());
		assert_eq!(j.batches().unwrap().last().unwrap().status, BatchStatus::Undone);
	}
	assert!(executed > 30, "only {executed} generated plans were valid");
}

/// Random damage appended to a journal never panics, never loses the valid batch before it, and
/// never lets a later batch's records go missing.
#[test]
fn journal_survives_random_damage() {
	let mut rng = Rng(42);
	for case in 0..100 {
		let s = Sandbox::new();
		s.tree();
		let v = s.validator();
		let mut j = s.journal();
		let first = Executor::new(&v, &mut j, s.trash())
			.execute(&Plan::new(vec![Op::Mkdir { path: s.root.join("one") }], ""), Source::Human)
			.unwrap();
		drop(j);

		let path = s.state.join("journal.jsonl");
		let mut bytes = fs::read(&path).unwrap();
		for _ in 0..rng.next() % 5 {
			let len = rng.next() % 300;
			bytes.extend((0..len).map(|_| (rng.next() % 256) as u8));
			if rng.next().is_multiple_of(2) {
				bytes.push(b'\n');
			}
		}
		fs::write(&path, &bytes).unwrap();

		let mut j = s.journal();
		let second = Executor::new(&v, &mut j, s.trash())
			.execute(&Plan::new(vec![Op::Mkdir { path: s.root.join("two") }], ""), Source::Human)
			.unwrap();
		let batches = j.batches().unwrap();
		let ids: Vec<&str> = batches.iter().map(|b| b.id.as_str()).collect();
		assert_eq!(ids, [first.id.as_str(), second.id.as_str()], "case {case}");
		assert!(batches.iter().all(|b| b.status == BatchStatus::Committed && !b.damaged), "case {case}");
	}
}
