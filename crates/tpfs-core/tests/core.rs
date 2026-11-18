use std::{
	fs,
	path::{Path, PathBuf},
};

use tempfile::TempDir;
use tpfs_core::{
	BatchStatus, Entry, ExecError, Executor, Journal, Op, Plan, Source, Validator,
	risk::{self, Level},
};

struct Sandbox {
	_tmp: TempDir,
	root: PathBuf,
	state: PathBuf,
}

impl Sandbox {
	fn new() -> Self {
		let tmp = TempDir::new().unwrap();
		let root = tpfs_core::canonicalize(tmp.path()).unwrap().join("root");
		let state = tpfs_core::canonicalize(tmp.path()).unwrap().join("state");
		fs::create_dir(&root).unwrap();
		Self { _tmp: tmp, root, state }
	}

	fn file(&self, rel: &str, content: &str) -> PathBuf {
		let p = self.root.join(rel);
		fs::write(&p, content).unwrap();
		p
	}

	fn dir(&self, rel: &str) -> PathBuf {
		let p = self.root.join(rel);
		fs::create_dir_all(&p).unwrap();
		p
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

	/// Relative path -> content ("dir/" for directories), sorted; the whole tree under root.
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

fn messages(v: &Validator, plan: &Plan) -> Vec<String> {
	v.validate(plan).into_iter().map(|i| i.to_string()).collect()
}

#[test]
fn rejects_paths_outside_roots() {
	let s = Sandbox::new();
	let outside = s.root.parent().unwrap().join("outside.txt");
	fs::write(&outside, "x").unwrap();
	let m = messages(&s.validator(), &Plan::new(vec![Op::Trash { path: outside }], ""));
	assert!(m.len() == 1 && m[0].contains("Outside"), "{m:?}");
}

#[test]
fn rejects_dotdot_escape_and_relative_paths() {
	let s = Sandbox::new();
	let v = s.validator();
	let sneaky = s.root.join("..").join("escape");
	assert_eq!(v.validate(&Plan::new(vec![Op::Mkdir { path: sneaky }], "")).len(), 1);
	assert_eq!(v.validate(&Plan::new(vec![Op::Mkdir { path: "relative/dir".into() }], "")).len(), 1);
}

#[test]
fn rejects_existing_target_and_double_move() {
	let s = Sandbox::new();
	let v = s.validator();
	let (a, b) = (s.file("a.txt", "A"), s.file("b.txt", "B"));
	assert_eq!(v.validate(&Plan::new(vec![Op::Move { from: a.clone(), to: b }], "")).len(), 1);

	let issues = v.validate(&Plan::new(
		vec![Op::Move { from: a.clone(), to: s.root.join("c.txt") }, Op::Move { from: a, to: s.root.join("d.txt") }],
		"",
	));
	assert!(issues.len() == 1 && issues[0].index == Some(1), "{issues:?}");
}

#[test]
fn simulates_mkdir_before_move() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let dir = s.root.join("photos");
	let plan = Plan::new(vec![Op::Mkdir { path: dir.clone() }, Op::Move { from: a, to: dir.join("a.txt") }], "");
	assert!(s.validator().validate(&plan).is_empty());
}

#[test]
fn operation_limit_can_be_raised_for_big_user_plans() {
	let s = Sandbox::new();
	let ops: Vec<Op> = (0..tpfs_core::MAX_OPS + 1).map(|i| Op::Mkdir { path: s.root.join(format!("d{i}")) }).collect();
	let plan = Plan::new(ops, "");
	assert!(messages(&s.validator(), &plan)[0].contains("Too many operations"));
	assert!(s.validator().with_max_ops(20_000).validate(&plan).is_empty());
}

#[test]
fn rejects_self_nesting_bad_names_and_root_removal() {
	let s = Sandbox::new();
	let v = s.validator();
	let dir = s.dir("x");
	assert_eq!(v.validate(&Plan::new(vec![Op::Move { from: dir.clone(), to: dir.join("y") }], "")).len(), 1);
	let f = s.file("f.txt", "");
	assert_eq!(v.validate(&Plan::new(vec![Op::Rename { path: f, new_name: "../evil.txt".into() }], "")).len(), 1);
	assert_eq!(v.validate(&Plan::new(vec![Op::Trash { path: s.root.clone() }], "")).len(), 1);
}

#[cfg(unix)]
#[test]
fn rejects_paths_through_symlinks() {
	let s = Sandbox::new();
	let outside = s.root.parent().unwrap().join("secret");
	fs::create_dir(&outside).unwrap();
	fs::write(outside.join("key"), "k").unwrap();
	std::os::unix::fs::symlink(&outside, s.root.join("link")).unwrap();

	let v = s.validator();
	let through = s.root.join("link").join("key");
	let m = messages(&v, &Plan::new(vec![Op::Trash { path: through }], ""));
	assert!(m.len() == 1 && m[0].contains("symlink"), "{m:?}");

	// The link itself may be moved: the operation acts on the link, not on its target.
	assert!(v.validate(&Plan::new(vec![Op::Trash { path: s.root.join("link") }], "")).is_empty());
}

#[cfg(unix)]
#[test]
fn refuses_to_copy_special_files() {
	let s = Sandbox::new();
	let fifo = s.root.join("pipe");
	assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
	let m = messages(&s.validator(), &Plan::new(vec![Op::Copy { from: fifo, to: s.root.join("copy") }], ""));
	assert!(m.len() == 1 && m[0].contains("Special"), "{m:?}");
}

#[test]
fn execute_and_undo_restore_everything() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let b = s.file("b.txt", "B");
	let c = s.file("c.txt", "C");
	s.dir("sub");
	s.file("sub/inner.txt", "I");
	let target = s.root.join("target");

	let plan = Plan::new(
		vec![
			Op::Mkdir { path: target.clone() },
			Op::Move { from: a, to: target.join("a.txt") },
			Op::Copy { from: s.root.join("sub"), to: target.join("sub-copy") },
			Op::Rename { path: b, new_name: "renamed.txt".into() },
			Op::Trash { path: c.clone() },
		],
		"test",
	);
	let before = s.snapshot();

	let v = s.validator();
	let mut j = s.journal();
	let batch = Executor::new(&v, &mut j, s.trash()).execute(&plan, Source::Human).unwrap();
	assert_eq!(batch.steps, 5);
	assert_eq!(fs::read_to_string(target.join("a.txt")).unwrap(), "A");
	assert_eq!(fs::read_to_string(target.join("sub-copy/inner.txt")).unwrap(), "I");
	assert!(s.root.join("renamed.txt").exists() && !c.exists());

	let undone = Executor::new(&v, &mut j, s.trash()).undo_last().unwrap().unwrap();
	assert_eq!(undone.id, batch.id);
	assert_eq!(s.snapshot(), before);
	assert!(Executor::new(&v, &mut j, s.trash()).undo_last().unwrap().is_none());
}

#[test]
fn rejects_a_plan_whose_target_appeared_before_execution() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let moved = s.root.join("c.txt");
	let plan =
		Plan::new(vec![Op::Mkdir { path: s.root.join("t") }, Op::Move { from: a.clone(), to: moved.clone() }], "");
	let v = s.validator();
	assert!(v.validate(&plan).is_empty());

	fs::write(&moved, "someone else").unwrap();
	let mut j = s.journal();
	let err = Executor::new(&v, &mut j, s.trash()).execute(&plan, Source::Human).unwrap_err();
	assert!(matches!(err, ExecError::Rejected(_)), "{err}");
	assert_eq!(fs::read_to_string(&moved).unwrap(), "someone else");
	assert!(!s.root.join("t").exists() && a.exists());
}

#[test]
fn conflict_appearing_mid_plan_never_overwrites_and_rolls_back() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let dir = s.root.join("t");
	let copy = s.root.join("b.txt");
	let moved = s.root.join("c.txt");
	let plan = Plan::new(
		vec![
			Op::Mkdir { path: dir.clone() },
			Op::Copy { from: a.clone(), to: copy.clone() },
			Op::Move { from: a.clone(), to: moved.clone() },
		],
		"",
	);
	let before = s.snapshot();

	let v = s.validator();
	let mut j = s.journal();
	let racer = moved.clone();
	// Another program creates c.txt after step 3 was re-checked, right before it runs.
	let err = Executor::new(&v, &mut j, s.trash())
		.before_apply(move |i| {
			if i == 2 {
				fs::write(&racer, "someone else").unwrap();
			}
		})
		.execute(&plan, Source::Human)
		.unwrap_err();

	match err {
		ExecError::Failed { step: 2, rollback_errors, .. } => {
			assert!(rollback_errors.is_empty(), "{rollback_errors:?}")
		}
		other => panic!("expected a failed step 3, got {other}"),
	}
	assert_eq!(fs::read_to_string(&moved).unwrap(), "someone else", "the foreign file was overwritten");
	let mut after = s.snapshot();
	after.retain(|e| e != "c.txt=someone else");
	assert_eq!(after, before, "earlier steps were not rolled back");
	assert_eq!(j.batches().unwrap()[0].status, BatchStatus::Aborted);
}

#[test]
fn recover_rolls_back_a_batch_interrupted_by_a_crash() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let b = s.file("b.txt", "B");
	let before = s.snapshot();
	let dir = s.root.join("new");
	let moved = dir.join("a.txt");

	// Simulate a crash: step 0 done, step 1 performed on disk but its `done` never written, step 2 never started.
	let mut j = s.journal();
	let batch = "crashed".to_string();
	j.append(&Entry::Begin {
		batch: batch.clone(),
		at: 0,
		source: "human".into(),
		reason: "crash".into(),
		digest: String::new(),
	})
	.unwrap();
	j.append(&Entry::Intent {
		batch: batch.clone(),
		index: 0,
		forward: Op::Mkdir { path: dir.clone() },
		inverse: Op::Rmdir { path: dir.clone() },
	})
	.unwrap();
	fs::create_dir(&dir).unwrap();
	j.append(&Entry::Done { batch: batch.clone(), index: 0 }).unwrap();
	j.append(&Entry::Intent {
		batch: batch.clone(),
		index: 1,
		forward: Op::Move { from: a.clone(), to: moved.clone() },
		inverse: Op::Move { from: moved.clone(), to: a.clone() },
	})
	.unwrap();
	fs::rename(&a, &moved).unwrap();
	let _ = b;

	let v = Validator::new(["/"]).unwrap();
	let report = Executor::new(&v, &mut j, s.trash()).recover().unwrap();
	assert_eq!(report.rolled_back, vec![batch]);
	assert!(report.errors.is_empty(), "{:?}", report.errors);
	assert_eq!(s.snapshot(), before);
	assert_eq!(j.batches().unwrap()[0].status, BatchStatus::Aborted);
}

#[test]
fn journal_survives_a_torn_last_line_and_checks_crc() {
	let s = Sandbox::new();
	let v = s.validator();
	let mut j = s.journal();
	let batch = Executor::new(&v, &mut j, s.trash())
		.execute(&Plan::new(vec![Op::Mkdir { path: s.root.join("d") }], ""), Source::Human)
		.unwrap();
	drop(j);

	let path = s.state.join("journal.jsonl");
	let mut text = fs::read_to_string(&path).unwrap();
	text.push_str("deadbeef {\"kind\":\"commit\",\"batch\":\"forged\"}\n"); // wrong CRC
	text.push_str("0000"); // torn write
	fs::write(&path, text).unwrap();

	let mut j = s.journal();
	let batches = j.batches().unwrap();
	assert_eq!(batches.len(), 1);
	assert_eq!(batches[0].id, batch.id);
	assert_eq!(j.corrupt_lines(), 2);
}

#[test]
fn journal_is_exclusive() {
	let s = Sandbox::new();
	let _first = s.journal();
	assert!(Journal::open(s.state.join("journal.jsonl")).is_err());
}

#[test]
fn plan_json_is_tolerant_and_has_a_schema() {
	let plan: Plan = serde_json::from_str(
		r#"{"reason":"r","ops":[{"path":"/x","op":"trash"},{"op":"rename","path":"/a","newName":"b"}]}"#,
	)
	.unwrap();
	assert_eq!(plan.ops[0], Op::Trash { path: "/x".into() });
	assert_eq!(plan.ops[1], Op::Rename { path: "/a".into(), new_name: "b".into() });

	let schema = Plan::schema().to_string();
	assert!(schema.contains("rename") && schema.contains("newName"), "{schema}");
	assert_eq!(plan.digest(), plan.clone().digest());
	assert_ne!(plan.digest(), Plan::new(plan.ops[..1].to_vec(), "r").digest());
}

#[test]
fn preview_sanitises_hostile_names() {
	let plan = Plan::new(vec![Op::Trash { path: "/tmp/\x1b]52;c;ZXZpbA==\x07".into() }], "");
	let line = &plan.preview()[0];
	assert!(!line.contains('\x1b') && line.contains("\\u{1b}"), "{line}");
}

#[test]
fn risk_levels() {
	let s = Sandbox::new();
	let f = s.file("a.txt", "");
	let d = s.dir("folder");
	let low = Plan::new(vec![Op::Copy { from: f.clone(), to: s.root.join("b.txt") }], "");
	assert_eq!(risk::assess(&low, Source::Human).level, Level::Low);
	assert_eq!(risk::assess(&low, Source::Agent).level, Level::High);
	assert_eq!(risk::assess(&Plan::new(vec![Op::Trash { path: d }], ""), Source::Human).level, Level::High);
	let many: Vec<Op> = (0..25).map(|i| Op::Mkdir { path: s.root.join(format!("d{i}")) }).collect();
	assert_eq!(risk::assess(&Plan::new(many, ""), Source::Human).level, Level::Medium);
}

/// Reads go through the locked handle (Windows locks are mandatory) and appends still land at the end.
#[test]
fn journal_reads_and_appends_interleave() {
	let s = Sandbox::new();
	let mut j = s.journal();
	let plan = Plan::new(vec![Op::Mkdir { path: s.root.join("a") }], "one");
	{
		let v = s.validator();
		Executor::new(&v, &mut j, s.trash()).execute(&plan, Source::Human).unwrap();
	}
	assert_eq!(j.batches().unwrap().len(), 1);
	let plan = Plan::new(vec![Op::Mkdir { path: s.root.join("b") }], "two");
	{
		let v = s.validator();
		Executor::new(&v, &mut j, s.trash()).execute(&plan, Source::Human).unwrap();
	}
	let batches = j.batches().unwrap();
	assert_eq!(batches.len(), 2);
	assert_eq!(j.corrupt_lines(), 0);
	assert_eq!(batches.iter().map(|b| b.reason.clone()).collect::<Vec<_>>(), ["one", "two"]);
}

#[test]
fn risk_of_a_huge_plan_is_assessed_in_linear_time() {
	// 100 000 steps, half of them trash: the old pairwise scan took minutes here.
	let ops: Vec<Op> = (0..50_000)
		.flat_map(|i| {
			let p = PathBuf::from(format!("/nowhere/f{i}"));
			[Op::Trash { path: p.clone() }, Op::Move { from: PathBuf::from(format!("/nowhere/g{i}")), to: p }]
		})
		.collect();
	let started = std::time::Instant::now();
	let r = risk::assess(&Plan::new(ops, ""), Source::Human);
	assert!(r.reasons.iter().any(|x| x.contains("replaces 50000")), "{:?}", r.reasons);
	assert!(started.elapsed().as_secs() < 5, "took {:?}", started.elapsed());
}

#[test]
fn history_of_a_big_batch_reads_quickly() {
	let s = Sandbox::new();
	let n = 5_000;
	let ops: Vec<Op> = (0..n).map(|i| Op::Mkdir { path: s.root.join(format!("d{i}")) }).collect();
	let v = s.validator().with_max_ops(n);
	let mut j = s.journal();
	Executor::new(&v, &mut j, s.trash()).execute(&Plan::new(ops, "big"), Source::Human).unwrap();
	let started = std::time::Instant::now();
	let b = j.batches().unwrap();
	assert_eq!(b[0].steps.len(), n);
	assert!(b[0].steps.iter().all(|s| s.done));
	assert!(started.elapsed().as_secs() < 3, "took {:?}", started.elapsed());
}

#[test]
fn recover_last_only_touches_an_interrupted_newest_batch() {
	let s = Sandbox::new();
	let dir = s.root.join("half");
	let mut j = s.journal();
	let crashed = |j: &mut Journal, id: &str| {
		j.append(&Entry::Begin {
			batch: id.into(),
			at: 0,
			source: "human".into(),
			reason: "crash".into(),
			digest: String::new(),
		})
		.unwrap();
		j.append(&Entry::Intent {
			batch: id.into(),
			index: 0,
			forward: Op::Mkdir { path: dir.clone() },
			inverse: Op::Rmdir { path: dir.clone() },
		})
		.unwrap();
		fs::create_dir(&dir).unwrap();
		j.append(&Entry::Done { batch: id.into(), index: 0 }).unwrap();
	};
	crashed(&mut j, "old");
	// something newer happened after the interrupted batch
	let v = Validator::new([&s.root]).unwrap();
	let newer = s.root.join("newer");
	Executor::new(&v, &mut j, s.trash())
		.execute(&Plan::new(vec![Op::Mkdir { path: newer.clone() }], ""), Source::Human)
		.unwrap();

	let r = Executor::new(&v, &mut j, s.trash()).recover_last().unwrap();
	assert!(r.rolled_back.is_empty(), "an older interrupted batch must not be rolled back by Undo");
	assert!(dir.exists() && newer.exists());

	fs::remove_dir(&dir).unwrap();
	crashed(&mut j, "new");
	let r = Executor::new(&v, &mut j, s.trash()).recover_last().unwrap();
	assert_eq!(r.rolled_back, vec!["new".to_string()]);
	assert!(!dir.exists() && newer.exists());
}
