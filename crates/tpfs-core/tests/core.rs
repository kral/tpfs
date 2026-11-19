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
	j.append(&Entry::Done { batch: batch.clone(), index: 0, check: None }).unwrap();
	j.append(&Entry::Intent {
		batch: batch.clone(),
		index: 1,
		forward: Op::Move { from: a.clone(), to: moved.clone() },
		inverse: Op::Move { from: moved.clone(), to: a.clone() },
	})
	.unwrap();
	fs::rename(&a, &moved).unwrap();
	let _ = b;

	let v = Validator::new(tpfs_core::system_roots()).unwrap();
	let report = Executor::new(&v, &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.is_empty(), "{:?}", report.errors);
	assert_eq!(report.rolled_back, vec![batch]);
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
fn a_batch_after_a_torn_tail_is_not_lost() {
	let s = Sandbox::new();
	let v = s.validator();
	let mut j = s.journal();
	let first = Executor::new(&v, &mut j, s.trash())
		.execute(&Plan::new(vec![Op::Mkdir { path: s.root.join("a") }], ""), Source::Human)
		.unwrap();
	drop(j);

	// Crash mid-append: a fragment without newline, cutting a multi-byte character in half.
	let path = s.state.join("journal.jsonl");
	let mut bytes = fs::read(&path).unwrap();
	bytes.extend_from_slice(b"0badc0de {\"kind\":\"begin\",\"reason\":\"lo\xC5");
	fs::write(&path, bytes).unwrap();

	let mut j = s.journal();
	let second = Executor::new(&v, &mut j, s.trash())
		.execute(&Plan::new(vec![Op::Mkdir { path: s.root.join("b") }], ""), Source::Human)
		.unwrap();
	let batches = j.batches().unwrap();
	assert_eq!(batches.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(), [first.id.as_str(), second.id.as_str()]);
	assert_eq!(batches[1].status, BatchStatus::Committed);
	assert_eq!(j.corrupt_lines(), 1);

	// Undo reverts the newest batch, not the one before the damage.
	Executor::new(&v, &mut j, s.trash()).undo_last().unwrap().unwrap();
	assert!(s.root.join("a").is_dir());
	assert!(!s.root.join("b").exists());
}

#[test]
fn journal_skips_oversized_and_non_utf8_lines() {
	let s = Sandbox::new();
	let v = s.validator();
	let mut j = s.journal();
	let batch = Executor::new(&v, &mut j, s.trash())
		.execute(&Plan::new(vec![Op::Mkdir { path: s.root.join("d") }], ""), Source::Human)
		.unwrap();
	drop(j);

	let path = s.state.join("journal.jsonl");
	let mut bytes = fs::read(&path).unwrap();
	bytes.extend(std::iter::repeat_n(b'x', 3 << 20));
	bytes.push(b'\n');
	bytes.extend_from_slice(b"00000000 \xFF\xFE\n");
	fs::write(&path, bytes).unwrap();

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
		j.append(&Entry::Done { batch: id.into(), index: 0, check: None }).unwrap();
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

#[test]
#[cfg(target_os = "linux")]
fn non_unicode_names_are_rejected_without_a_panic() {
	use std::os::unix::ffi::OsStrExt;
	let s = Sandbox::new();
	let name = std::ffi::OsStr::from_bytes(b"caf\xe9.txt");
	let path = s.root.join(name);
	fs::write(&path, "x").unwrap();
	let plan = Plan::new(vec![Op::Trash { path: path.clone() }], "");
	let digest = plan.digest();
	assert_eq!(digest.len(), 64);
	assert!(messages(&s.validator(), &plan)[0].contains("not valid Unicode"));
	let mut j = s.journal();
	assert!(matches!(
		Executor::new(&s.validator(), &mut j, s.trash()).execute(&plan, Source::Human),
		Err(ExecError::Rejected(_))
	));
	assert!(path.exists());
}

#[test]
fn plans_cannot_touch_tpfs_state() {
	let s = Sandbox::new();
	let base = s.root.parent().unwrap().to_path_buf();
	let mut j = s.journal(); // creates the state folder
	let v = Validator::new([&base]).unwrap().with_protected([&s.state]);

	let journal = s.state.join("journal.jsonl");
	let evil = s.file("evil.jsonl", "forged");
	for (plan, expect) in [
		(vec![Op::Trash { path: journal.clone() }], "tpfs's state"),
		(vec![Op::Copy { from: evil.clone(), to: s.state.join("pending.approved") }], "tpfs's state"),
		(vec![Op::Mkdir { path: s.root.join(".tpfs-part-x") }], "own working data"),
	] {
		let plan = Plan::new(plan, "");
		assert!(messages(&v, &plan)[0].contains(expect), "{:?}", messages(&v, &plan));
	}

	let keep = s.dir("keep");
	let nested = Validator::new([&base]).unwrap().with_protected([keep.join("state")]);
	let plan = Plan::new(vec![Op::Trash { path: keep.clone() }], "");
	assert!(messages(&nested, &plan)[0].contains("contains tpfs's state"), "{:?}", messages(&nested, &plan));

	// The executor protects its own state even when the caller's validator does not.
	let plain = Validator::new([&base]).unwrap();
	let plan = Plan::new(vec![Op::Trash { path: journal.clone() }], "");
	assert!(matches!(
		Executor::new(&plain, &mut j, s.trash()).execute(&plan, Source::Agent),
		Err(ExecError::Rejected(_))
	));
	assert!(journal.exists());
}

#[test]
#[cfg(unix)]
fn state_folder_must_be_private() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	fs::create_dir(&s.state).unwrap();
	fs::set_permissions(&s.state, fs::Permissions::from_mode(0o777)).unwrap();
	assert!(Journal::open(s.state.join("journal.jsonl")).is_err());

	let real = s.root.parent().unwrap().join("real");
	fs::create_dir(&real).unwrap();
	fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
	let link = s.root.parent().unwrap().join("link");
	std::os::unix::fs::symlink(&real, &link).unwrap();
	assert!(Journal::open(link.join("journal.jsonl")).is_err());

	// A journal that is a symlink is refused too.
	let ok = s.root.parent().unwrap().join("ok");
	fs::create_dir(&ok).unwrap();
	fs::set_permissions(&ok, fs::Permissions::from_mode(0o700)).unwrap();
	std::os::unix::fs::symlink(real.join("x"), ok.join("journal.jsonl")).unwrap();
	assert!(Journal::open(ok.join("journal.jsonl")).is_err());
}

fn run(s: &Sandbox, ops: Vec<Op>) -> String {
	let mut j = s.journal();
	Executor::new(&s.validator(), &mut j, s.trash()).execute(&Plan::new(ops, ""), Source::Human).unwrap().id
}

fn undo(s: &Sandbox) -> Result<Option<tpfs_core::BatchReport>, ExecError> {
	let mut j = s.journal();
	Executor::new(&s.validator(), &mut j, s.trash()).undo_last()
}

#[test]
fn undo_keeps_a_copy_that_was_changed_since() {
	let s = Sandbox::new();
	let a = s.file("report.txt", "v1");
	let b = s.root.join("backup.txt");
	run(&s, vec![Op::Copy { from: a, to: b.clone() }]);
	std::thread::sleep(std::time::Duration::from_millis(20));
	fs::write(&b, "edited by the user").unwrap();

	let err = undo(&s).unwrap_err();
	assert!(matches!(err, ExecError::Undo { .. }) && err.to_string().contains("changed"), "{err}");
	assert_eq!(fs::read_to_string(&b).unwrap(), "edited by the user");

	// Once the user moved it away, undo completes.
	fs::rename(&b, s.root.join("mine.txt")).unwrap();
	assert_eq!(undo(&s).unwrap().unwrap().steps, 1);
}

#[test]
fn undo_waits_for_an_interrupted_newer_batch() {
	let s = Sandbox::new();
	run(&s, vec![Op::Mkdir { path: s.root.join("older") }]);
	let mut j = s.journal();
	j.append(&Entry::Begin {
		batch: "x".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
	})
	.unwrap();
	drop(j);
	let err = undo(&s).unwrap_err();
	assert!(err.to_string().contains("recover"), "{err}");
	assert!(s.root.join("older").is_dir(), "the older batch must not be undone instead");
}

#[test]
fn undo_resumes_after_a_crash_between_an_inverse_and_its_record() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let d = s.root.join("d");
	run(&s, vec![Op::Mkdir { path: d.clone() }, Op::Move { from: a.clone(), to: d.join("a.txt") }]);
	// The crash: the last step's inverse ran, its undo_step record was never written.
	fs::rename(d.join("a.txt"), &a).unwrap();
	assert_eq!(undo(&s).unwrap().unwrap().steps, 2);
	assert!(a.exists() && !d.exists());
}

#[test]
#[cfg(unix)]
fn undo_does_not_follow_a_planted_symlink() {
	let s = Sandbox::new();
	let src = s.file("f", "f");
	let sub = s.dir("sub");
	run(&s, vec![Op::Copy { from: src, to: sub.join("precious") }]);
	let outside = s.root.parent().unwrap().join("outside");
	fs::create_dir(&outside).unwrap();
	fs::write(outside.join("precious"), "not tpfs's").unwrap();
	fs::rename(&sub, s.root.join("sub-real")).unwrap();
	std::os::unix::fs::symlink(&outside, &sub).unwrap();

	assert!(undo(&s).is_err());
	assert_eq!(fs::read_to_string(outside.join("precious")).unwrap(), "not tpfs's");
}

#[test]
fn recover_is_idempotent_and_retryable() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let mut j = s.journal();
	let inverse = Op::Move { from: s.root.join("b.txt"), to: a.clone() };
	j.append(&Entry::Begin {
		batch: "x".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
	})
	.unwrap();
	j.append(&Entry::Intent {
		batch: "x".into(),
		index: 0,
		forward: Op::Move { from: a.clone(), to: s.root.join("b.txt") },
		inverse,
	})
	.unwrap();
	j.append(&Entry::Done { batch: "x".into(), index: 0, check: None }).unwrap();
	drop(j);
	fs::rename(&a, s.root.join("b.txt")).unwrap();
	// Something now sits where the file has to go back: recovery cannot finish yet.
	fs::write(&a, "blocker").unwrap();

	let v = s.validator();
	let mut j = s.journal();
	let report = Executor::new(&v, &mut j, s.trash()).recover().unwrap();
	assert!(!report.errors.is_empty() && report.rolled_back.is_empty());
	assert_eq!(j.batches().unwrap()[0].status, BatchStatus::Incomplete);

	fs::remove_file(&a).unwrap();
	for _ in 0..2 {
		let report = Executor::new(&v, &mut j, s.trash()).recover().unwrap();
		assert!(report.errors.is_empty(), "{:?}", report.errors);
	}
	assert_eq!(fs::read_to_string(&a).unwrap(), "A");
	assert_eq!(j.batches().unwrap()[0].status, BatchStatus::Aborted);
}

/// On case-insensitive file systems another spelling of a root is still that root.
#[test]
#[cfg(any(windows, target_os = "macos"))]
fn another_spelling_of_a_root_is_the_root() {
	let s = Sandbox::new();
	let photos = s.dir("Photos");
	let v = Validator::new([&s.root, &photos]).unwrap();
	let mut spellings = vec![s.root.join("photos"), s.root.join("PHOTOS")];
	if cfg!(windows) {
		spellings.push(s.root.join("Photos."));
	}
	for p in spellings {
		let plan = Plan::new(vec![Op::Trash { path: p.clone() }], "");
		let m = messages(&v, &plan);
		assert!(!m.is_empty(), "{} was accepted", p.display());
	}
	assert!(photos.is_dir());
}

#[test]
#[cfg(windows)]
fn windows_rejects_names_it_would_misread() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "a");
	for name in ["CON", "nul.txt", "COM1", "x.", "x ", "a.txt:hidden"] {
		let plan = Plan::new(vec![Op::Copy { from: a.clone(), to: s.root.join(name) }], "");
		assert!(!messages(&s.validator(), &plan).is_empty(), "{name} was accepted");
	}
}
