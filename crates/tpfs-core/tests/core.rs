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
	// mknodat, not a `mkfifo` process: a forked child briefly holds other tests' journal locks.
	{
		use std::os::unix::ffi::OsStrExt;
		let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
		// SAFETY: `c` is a valid NUL-terminated path that outlives the call.
		assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
	}
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
		v: 1,
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
	j.append(&Entry::Done { batch: batch.clone(), index: 0, check: None, mode: None }).unwrap();
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

	{
		let schema = Plan::schema().to_string();
		assert!(schema.contains("rename") && schema.contains("newName"), "{schema}");
		assert_eq!(plan.digest(), plan.clone().digest());
		assert_ne!(plan.digest(), Plan::new(plan.ops[..1].to_vec(), "r").digest());
	}
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
	// Generous: catches quadratic behaviour (minutes), not slow CI machines.
	assert!(started.elapsed().as_secs() < 30, "took {:?}", started.elapsed());
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
	assert!(started.elapsed().as_secs() < 30, "took {:?}", started.elapsed());
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
			v: 1,
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
		j.append(&Entry::Done { batch: id.into(), index: 0, check: None, mode: None }).unwrap();
	};
	crashed(&mut j, "old");
	let v = Validator::new([&s.root]).unwrap();
	// A new batch on top of an interrupted one is refused…
	let newer = s.root.join("newer");
	let plan = Plan::new(vec![Op::Mkdir { path: newer.clone() }], "");
	assert!(matches!(Executor::new(&v, &mut j, s.trash()).execute(&plan, Source::Human), Err(ExecError::Blocked(_))));
	// …but an old journal may already have one (written before this rule): simulate it.
	j.append(&Entry::Begin {
		batch: "n".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	j.append(&Entry::Intent {
		batch: "n".into(),
		index: 0,
		forward: Op::Mkdir { path: newer.clone() },
		inverse: Op::Rmdir { path: newer.clone() },
	})
	.unwrap();
	fs::create_dir(&newer).unwrap();
	j.append(&Entry::Done { batch: "n".into(), index: 0, check: None, mode: None }).unwrap();
	j.append(&Entry::Commit { batch: "n".into() }).unwrap();

	let r = Executor::new(&v, &mut j, s.trash()).recover_last().unwrap();
	assert!(r.rolled_back.is_empty(), "an older interrupted batch must not be rolled back by Undo");
	let r = Executor::new(&v, &mut j, s.trash()).recover().unwrap();
	assert!(r.rolled_back.is_empty() && r.errors[0].contains("newer batches"), "{r:?}");
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
		(vec![Op::Mkdir { path: s.root.join(".tpfs-part-18e95b2d1cb586a8-0") }], "own working data"),
	] {
		let plan = Plan::new(plan, "");
		assert!(messages(&v, &plan)[0].contains(expect), "{:?}", messages(&v, &plan));
	}

	// Another spelling of the state folder is the same folder on a volume that ignores case.
	let name = s.state.file_name().unwrap().to_string_lossy().to_uppercase();
	let other = s.state.with_file_name(&name);
	for op in [Op::Trash { path: other.clone() }, Op::Move { from: other.join("journal.jsonl"), to: s.root.join("j") }]
	{
		let plan = Plan::new(vec![op], "");
		assert!(messages(&v, &plan).iter().any(|m| m.contains("tpfs's state")), "{:?}", messages(&v, &plan));
	}
	for n in [
		".tpf\u{17f}-trash-1000",
		".TPF\u{17f}-PART-0123abcd-0",
		".tpfs-trash-1000.",
		".tpfs-trash-1000. .",
		".tpfs-trash-1000 ",
		".tpfs-case-0123abcd-0..",
	] {
		assert!(tpfs_core::is_own_name(n.as_ref()), "{n}");
	}

	// …and so is a spelling with trailing dots, which FAT, exFAT and Windows ignore.
	let dotted = s.state.with_file_name(format!("{}.", s.state.file_name().unwrap().to_string_lossy()));
	let plan = Plan::new(vec![Op::Trash { path: dotted }], "");
	// (Windows refuses such a name before it gets that far.)
	assert!(
		messages(&v, &plan).iter().any(|m| m.contains("tpfs's state") || m.contains("cannot use this name")),
		"{:?}",
		messages(&v, &plan)
	);

	let keep = s.dir("keep");
	let nested = Validator::new([&base]).unwrap().with_protected([keep.join("state")]);
	let plan = Plan::new(vec![Op::Trash { path: keep.clone() }], "");
	assert!(messages(&nested, &plan)[0].contains("contains tpfs's state"), "{:?}", messages(&nested, &plan));
	let plan = Plan::new(vec![Op::Trash { path: keep.with_file_name("KEEP") }], "");
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
		v: 1,
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
		v: 1,
	})
	.unwrap();
	j.append(&Entry::Intent {
		batch: "x".into(),
		index: 0,
		forward: Op::Move { from: a.clone(), to: s.root.join("b.txt") },
		inverse,
	})
	.unwrap();
	j.append(&Entry::Done { batch: "x".into(), index: 0, check: None, mode: None }).unwrap();
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

#[test]
fn damage_inside_a_batch_stops_automatic_recovery() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let b = s.root.join("b.txt");
	let mut j = s.journal();
	j.append(&Entry::Begin {
		batch: "x".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	drop(j);
	// An intent was lost to damage; a later record of the batch survived.
	let path = s.state.join("journal.jsonl");
	let mut bytes = fs::read(&path).unwrap();
	bytes.extend_from_slice(b"garbage that was an intent\n");
	fs::write(&path, bytes).unwrap();
	let mut j = s.journal();
	j.append(&Entry::Done { batch: "x".into(), index: 0, check: None, mode: None }).unwrap();
	fs::rename(&a, &b).unwrap(); // whatever the lost step did

	assert!(j.batches().unwrap()[0].damaged);
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(report.rolled_back.is_empty() && report.errors[0].contains("damaged"), "{report:?}");
	assert!(b.exists(), "nothing is guessed");
}

#[test]
fn preview_and_risk_see_the_path_that_is_acted_on() {
	let s = Sandbox::new();
	s.dir("Photos");
	s.dir("tmp");
	let sneaky = s.root.join("tmp").join("..").join("Photos");
	let plan = Plan::new(vec![Op::Trash { path: sneaky }], "");
	let line = &plan.preview()[0];
	assert!(!line.contains("..") && line.ends_with("Photos"), "{line}");
	assert_eq!(tpfs_core::risk::assess(&plan, Source::Human).level, tpfs_core::risk::Level::High);
}

#[test]
fn a_huge_reason_is_rejected_before_anything_is_journaled() {
	let s = Sandbox::new();
	let plan = Plan::new(vec![Op::Mkdir { path: s.root.join("d") }], "\u{1}".repeat(tpfs_core::MAX_REASON + 1));
	assert!(messages(&s.validator(), &plan)[0].contains("too long"));
	let mut j = s.journal();
	assert!(matches!(
		Executor::new(&s.validator(), &mut j, s.trash()).execute(&plan, Source::Human),
		Err(ExecError::Rejected(_))
	));
	assert!(j.batches().unwrap().is_empty());
}

#[test]
fn a_lost_commit_or_begin_is_damage_not_a_reason_to_roll_back_or_skip() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let b = s.root.join("b.txt");
	run(&s, vec![Op::Move { from: a.clone(), to: b.clone() }]);
	let path = s.state.join("journal.jsonl");
	// Damage the commit line of that batch, then let another batch follow.
	let text = fs::read_to_string(&path).unwrap();
	let damaged: String = text
		.lines()
		.map(|l| if l.contains("\"commit\"") { "xxxxxxxx garbage\n".to_owned() } else { format!("{l}\n") })
		.collect();
	fs::write(&path, damaged).unwrap();
	let mut j = s.journal();
	j.append(&Entry::Begin {
		batch: "later".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	j.append(&Entry::Commit { batch: "later".into() }).unwrap();
	let batches = j.batches().unwrap();
	assert!(batches[0].damaged, "{batches:?}");
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(report.rolled_back.is_empty(), "a committed batch whose commit line was lost must not be rolled back");
	assert!(b.exists() && !a.exists());

	// A batch whose begin is unreadable stays visible (damaged) and undo does not skip past it.
	let s = Sandbox::new();
	run(&s, vec![Op::Mkdir { path: s.root.join("first") }]);
	run(&s, vec![Op::Mkdir { path: s.root.join("second") }]);
	let text = fs::read_to_string(path_of(&s)).unwrap();
	let mut lines: Vec<&str> = text.lines().collect();
	let second_begin = lines.iter().rposition(|l| l.contains("\"begin\"")).unwrap();
	lines[second_begin] = "0 unreadable";
	fs::write(path_of(&s), lines.join("\n") + "\n").unwrap();
	let err = undo(&s).unwrap_err();
	assert!(err.to_string().contains("damaged"), "{err}");
	assert!(s.root.join("first").is_dir() && s.root.join("second").is_dir());
}

fn path_of(s: &Sandbox) -> PathBuf {
	s.state.join("journal.jsonl")
}

#[test]
fn undo_finishes_what_an_interrupted_undo_started() {
	// A move whose undo crashed after the copy back was published, with the far side half removed.
	let s = Sandbox::new();
	let dir = s.dir("dir");
	fs::write(dir.join("a"), "A").unwrap();
	fs::write(dir.join("b"), "B").unwrap();
	let away = s.root.join("away");
	let id = run(&s, vec![Op::Move { from: dir.clone(), to: away.clone() }]);
	// The interrupted undo: the copy is back at `dir`, `away` lost one entry already.
	fs::create_dir(&dir).unwrap();
	fs::write(dir.join("a"), "A").unwrap();
	fs::write(dir.join("b"), "B").unwrap();
	fs::remove_file(away.join("b")).unwrap();
	let mut j = s.journal();
	j.append(&Entry::UndoPublished { batch: id.clone(), index: 0 }).unwrap();
	drop(j);

	assert_eq!(undo(&s).unwrap().unwrap().steps, 1);
	assert!(!away.exists());
	assert_eq!(fs::read_to_string(dir.join("b")).unwrap(), "B");
}

#[test]
fn undo_drops_a_stale_partial_copy_and_completes_a_stuck_case_rename() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let id = run(&s, vec![Op::Rename { path: a.clone(), new_name: "renamed.txt".into() }]);
	// An earlier undo stopped between the two renames of a letter-case style rename.
	let tmp = s.root.join(format!(".tpfs-case-{id}-undo-0"));
	fs::rename(s.root.join("renamed.txt"), &tmp).unwrap();
	assert_eq!(undo(&s).unwrap().unwrap().steps, 1);
	assert_eq!(fs::read_to_string(&a).unwrap(), "A");
	assert!(!tmp.exists());

	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let b = s.root.join("b.txt");
	let id = run(&s, vec![Op::Move { from: a.clone(), to: b.clone() }]);
	fs::write(s.root.join(format!(".tpfs-part-{id}-undo-0")), "half a copy").unwrap();
	assert_eq!(undo(&s).unwrap().unwrap().steps, 1);
	assert!(a.exists() && !s.root.join(format!(".tpfs-part-{id}-undo-0")).exists());
}

#[test]
fn a_plan_may_change_its_own_copy_and_still_be_undone() {
	let s = Sandbox::new();
	let src = s.dir("src");
	fs::write(src.join("f"), "f").unwrap();
	let x = s.file("x.txt", "x");
	let copy = s.root.join("copy");
	run(
		&s,
		vec![
			Op::Copy { from: src, to: copy.clone() },
			Op::Mkdir { path: copy.join("sub") },
			Op::Move { from: x.clone(), to: copy.join("x.txt") },
		],
	);
	assert_eq!(undo(&s).unwrap().unwrap().steps, 3);
	assert!(!copy.exists() && x.exists());
}

#[test]
fn recover_keeps_a_copy_edited_after_the_crash() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let b = s.root.join("b.txt");
	fs::copy(&a, &b).unwrap();
	let mut j = s.journal();
	j.append(&Entry::Begin {
		batch: "x".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	j.append(&Entry::Intent {
		batch: "x".into(),
		index: 0,
		forward: Op::Copy { from: a.clone(), to: b.clone() },
		inverse: Op::Trash { path: b.clone() },
	})
	.unwrap();
	j.append(&Entry::Done { batch: "x".into(), index: 0, check: None, mode: None }).unwrap();
	j.append(&Entry::Check { batch: "x".into(), index: 0, check: "fingerprint of the original copy".into() }).unwrap();
	fs::write(&b, "edited after the crash").unwrap();
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.iter().any(|e| e.contains("changed")), "{report:?}");
	assert_eq!(fs::read_to_string(&b).unwrap(), "edited after the crash");
}

#[test]
#[cfg(unix)]
fn undoing_a_folder_removal_restores_its_permissions() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	let d = s.dir("private");
	fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
	run(&s, vec![Op::Rmdir { path: d.clone() }]);
	undo(&s).unwrap().unwrap();
	assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
}

#[test]
#[cfg(unix)]
fn undo_works_with_a_symlinked_state_folder() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	let base = s.root.parent().unwrap();
	let real = base.join("real-state");
	fs::create_dir(&real).unwrap();
	fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
	let parent_link = base.join("link-parent");
	std::os::unix::fs::symlink(base, &parent_link).unwrap();
	// The state folder is reached through a symlinked parent, like /home -> /var/home.
	let state = parent_link.join("real-state");
	let f = s.file("f.txt", "F");
	let v = Validator::new(tpfs_core::system_roots()).unwrap();
	let mut j = Journal::open(state.join("journal.jsonl")).unwrap();
	Executor::new(&v, &mut j, state.join("trash"))
		.execute(&Plan::new(vec![Op::Trash { path: f.clone() }], ""), Source::Human)
		.unwrap();
	Executor::new(&v, &mut j, state.join("trash")).undo_last().unwrap().unwrap();
	assert_eq!(fs::read_to_string(&f).unwrap(), "F");
}

#[test]
fn a_journaled_path_that_only_looks_like_a_trash_is_still_checked() {
	for planted in [".tpfs-trash-x".to_owned(), own_trash_name()] {
		a_planted_trash_is_not_trusted(&planted);
	}
}

/// This user's per-file-system trash name (Unix), which a planted folder may copy.
fn own_trash_name() -> String {
	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt;
		let probe = tempfile::NamedTempFile::new().unwrap();
		format!(".tpfs-trash-{}", fs::metadata(probe.path()).unwrap().uid())
	}
	#[cfg(not(unix))]
	".tpfs-trash-0".to_owned()
}

/// A folder with the trash's name outside the roots, but not private (0o755): a journaled path
/// into it is checked like any other, and nothing is made there.
fn a_planted_trash_is_not_trusted(name: &str) {
	let s = Sandbox::new();
	let outside = s.root.parent().unwrap().join("outside");
	fs::create_dir_all(outside.join(name)).unwrap();
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(outside.join(name), fs::Permissions::from_mode(0o755)).unwrap();
	}
	let mut j = s.journal();
	j.append(&Entry::Begin {
		batch: "x".into(),
		at: 0,
		source: "agent".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	let target = outside.join(name).join("planted");
	j.append(&Entry::Intent {
		batch: "x".into(),
		index: 0,
		forward: Op::Mkdir { path: target.clone() },
		inverse: Op::Mkdir { path: target.clone() },
	})
	.unwrap();
	j.append(&Entry::Done { batch: "x".into(), index: 0, check: None, mode: None }).unwrap();
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(!report.errors.is_empty() && !target.exists(), "{report:?}");
}

#[test]
#[cfg(windows)]
fn windows_long_paths_and_shares() {
	assert_eq!(tpfs_core::share_root(Path::new(r"\\nas\photos\2024\a.jpg")), Some(PathBuf::from(r"\\nas\photos\")));
	assert_eq!(tpfs_core::share_root(Path::new(r"C:\x")), None);

	// A file deeper than 260 characters below a short root: validated, moved and undone.
	let s = Sandbox::new();
	let mut deep = s.root.clone();
	while deep.as_os_str().len() < 300 {
		deep.push("a-rather-long-folder-name");
	}
	fs::create_dir_all(&deep).unwrap();
	let f = deep.join("f.txt");
	fs::write(&f, "F").unwrap();
	let moved = deep.join("g.txt");
	let plan = Plan::new(vec![Op::Move { from: f.clone(), to: moved.clone() }], "");
	assert!(messages(&s.validator(), &plan).is_empty(), "{:?}", messages(&s.validator(), &plan));
	run(&s, plan.ops.clone());
	assert!(moved.exists());
	undo(&s).unwrap().unwrap();
	assert!(f.exists());
}

#[test]
#[cfg(unix)]
fn recover_does_not_follow_a_planted_symlink() {
	let s = Sandbox::new();
	let outside = s.root.parent().unwrap().join("outside");
	fs::create_dir_all(&outside).unwrap();
	fs::write(outside.join("x"), "not tpfs's").unwrap();
	std::os::unix::fs::symlink(&outside, s.root.join("link")).unwrap();
	let a = s.root.join("a");
	let mut j = s.journal();
	j.append(&Entry::Begin {
		batch: "x".into(),
		at: 0,
		source: "agent".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	j.append(&Entry::Intent {
		batch: "x".into(),
		index: 0,
		forward: Op::Move { from: a.clone(), to: s.root.join("link/x") },
		inverse: Op::Move { from: s.root.join("link/x"), to: a.clone() },
	})
	.unwrap();
	j.append(&Entry::Done { batch: "x".into(), index: 0, check: None, mode: None }).unwrap();
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(!report.errors.is_empty() && report.rolled_back.is_empty(), "{report:?}");
	assert_eq!(fs::read_to_string(outside.join("x")).unwrap(), "not tpfs's");
	assert!(!a.exists());
	assert_eq!(j.batches().unwrap()[0].status, BatchStatus::Incomplete);
}

#[test]
fn recover_merges_back_a_cross_device_move_that_stopped_while_removing_the_source() {
	// The state after a crash: the copy was published at the target, the source is half removed
	// and got a new file meanwhile.
	let s = Sandbox::new();
	let (src, dst) = (s.root.join("src"), s.root.join("dst"));
	fs::create_dir_all(dst.join("sub")).unwrap();
	fs::write(dst.join("a"), "A").unwrap();
	fs::write(dst.join("sub/b"), "B").unwrap();
	fs::create_dir_all(&src).unwrap();
	fs::write(src.join("a"), "A").unwrap(); // not removed yet
	fs::write(src.join("late"), "appeared during the move").unwrap();
	let mut j = s.journal();
	j.append(&Entry::Begin {
		batch: "x".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	j.append(&Entry::Intent {
		batch: "x".into(),
		index: 0,
		forward: Op::Move { from: src.clone(), to: dst.clone() },
		inverse: Op::Move { from: dst.clone(), to: src.clone() },
	})
	.unwrap();
	j.append(&Entry::Published { batch: "x".into(), index: 0 }).unwrap();

	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.is_empty(), "{report:?}");
	assert_eq!(fs::read_to_string(src.join("sub/b")).unwrap(), "B");
	assert_eq!(fs::read_to_string(src.join("late")).unwrap(), "appeared during the move");
	assert!(!dst.exists(), "the duplicate went to the trash");
	assert_eq!(j.batches().unwrap()[0].status, BatchStatus::Aborted);
}

#[test]
fn recover_resumes_after_it_was_interrupted_itself() {
	let s = Sandbox::new();
	let (a, b) = (s.file("a.txt", "A"), s.file("b.txt", "B"));
	let (a2, b2) = (s.root.join("a2.txt"), s.root.join("b2.txt"));
	let mut j = s.journal();
	j.append(&Entry::Begin {
		batch: "x".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	for (i, (from, to)) in [(&a, &a2), (&b, &b2)].into_iter().enumerate() {
		j.append(&Entry::Intent {
			batch: "x".into(),
			index: i,
			forward: Op::Move { from: from.clone(), to: to.clone() },
			inverse: Op::Move { from: to.clone(), to: from.clone() },
		})
		.unwrap();
		fs::rename(from, to).unwrap();
		j.append(&Entry::Done { batch: "x".into(), index: i, check: None, mode: None }).unwrap();
	}
	// A first recovery reverted step 2 and then died.
	fs::rename(&b2, &b).unwrap();
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.is_empty(), "{report:?}");
	assert!(a.exists() && b.exists() && !a2.exists() && !b2.exists());
}

#[test]
fn a_source_that_vanishes_before_its_step_fails_cleanly() {
	let s = Sandbox::new();
	let before_files = (s.file("a.txt", "A"), s.file("b.txt", "B"));
	let before = s.snapshot();
	let plan = Plan::new(
		vec![
			Op::Mkdir { path: s.root.join("t") },
			Op::Move { from: before_files.1.clone(), to: s.root.join("t/b.txt") },
		],
		"",
	);
	let v = s.validator();
	let mut j = s.journal();
	let gone = before_files.1.clone();
	let err = Executor::new(&v, &mut j, s.trash())
		.before_apply(move |step| {
			if step == 0 {
				fs::remove_file(&gone).unwrap();
			}
		})
		.execute(&plan, Source::Human)
		.unwrap_err();
	assert!(matches!(err, ExecError::Failed { step: 1, .. }), "{err}");
	let mut expected = before;
	expected.retain(|l| !l.starts_with("b.txt"));
	assert_eq!(s.snapshot(), expected, "step 1 was rolled back");
}

#[test]
fn each_kind_of_invalid_step_is_named() {
	let s = Sandbox::new();
	let full = s.dir("full");
	s.file("full/x", "x");
	let f = s.file("f.txt", "f");
	for (op, expect) in [
		(Op::Rmdir { path: full.clone() }, "not empty"),
		(Op::Rmdir { path: f.clone() }, "not an existing directory"),
		(Op::Mkdir { path: s.root.join("missing/new") }, "does not exist"),
		(Op::Move { from: s.root.join("nope"), to: s.root.join("x") }, "does not exist"),
	] {
		let m = messages(&s.validator(), &Plan::new(vec![op.clone()], ""));
		assert!(m.len() == 1 && m[0].to_lowercase().contains(expect), "{op}: {m:?}");
	}
	assert!(messages(&s.validator(), &Plan::new(vec![], ""))[0].contains("empty"));
}

#[test]
#[cfg(unix)]
fn copies_never_keep_setuid_or_setgid() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	let bin = s.file("tool", "#!/bin/sh\n");
	fs::set_permissions(&bin, fs::Permissions::from_mode(0o6755)).unwrap();
	let dir = s.dir("pkg");
	let inner = dir.join("inner");
	fs::write(&inner, "x").unwrap();
	fs::set_permissions(&inner, fs::Permissions::from_mode(0o4711)).unwrap();
	run(
		&s,
		vec![Op::Copy { from: bin, to: s.root.join("tool-copy") }, Op::Copy { from: dir, to: s.root.join("pkg-copy") }],
	);
	for p in [s.root.join("tool-copy"), s.root.join("pkg-copy/inner")] {
		let mode = fs::metadata(&p).unwrap().permissions().mode();
		assert_eq!(mode & 0o6000, 0, "{}: {mode:o}", p.display());
	}
	assert_eq!(fs::metadata(s.root.join("tool-copy")).unwrap().permissions().mode() & 0o777, 0o755);
}

#[test]
fn recovery_records_after_a_torn_tail_are_not_damage() {
	let s = Sandbox::new();
	let mut j = s.journal();
	j.append(&Entry::Begin {
		batch: "x".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	drop(j);
	let path = s.state.join("journal.jsonl");
	let mut bytes = fs::read(&path).unwrap();
	bytes.extend_from_slice(b"0000 {\"kind\":\"intent\""); // torn while writing an intent
	fs::write(&path, bytes).unwrap();
	let mut j = s.journal();
	// A first recovery wrote an intermediate record, then needed a second run.
	j.append(&Entry::UndoPublished { batch: "x".into(), index: 0 }).unwrap();
	assert!(!j.batches().unwrap()[0].damaged);
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.is_empty() && report.rolled_back == ["x"], "{report:?}");
}

#[test]
fn undo_redoes_a_move_back_that_was_recorded_but_not_yet_renamed() {
	// The record "about to publish" is written before the rename: a crash in between leaves the
	// complete copy under its part name, and the original untouched.
	let s = Sandbox::new();
	let dir = s.dir("dir");
	fs::write(dir.join("a"), "A").unwrap();
	let away = s.root.join("away");
	let id = run(&s, vec![Op::Move { from: dir.clone(), to: away.clone() }]);
	let part = s.root.join(format!(".tpfs-part-{id}-undo-0"));
	fs::create_dir(&part).unwrap();
	fs::write(part.join("a"), "A").unwrap();
	let mut j = s.journal();
	j.append(&Entry::UndoPublished { batch: id.clone(), index: 0 }).unwrap();
	drop(j);
	assert_eq!(undo(&s).unwrap().unwrap().steps, 1);
	assert!(!away.exists() && !part.exists());
	assert_eq!(fs::read_to_string(dir.join("a")).unwrap(), "A");
}

#[test]
fn undo_never_trusts_a_look_alike_at_the_old_place() {
	// Someone plants a file with the same size and time where the moved one came from: undo must
	// refuse, not trash the user's real file and keep the planted one.
	let s = Sandbox::new();
	let a = s.file("report.txt", "real numbers");
	let b = s.root.join("mine.txt");
	run(&s, vec![Op::Move { from: a.clone(), to: b.clone() }]);
	fs::write(&a, "EVIL numbers").unwrap();
	let t = fs::metadata(&b).unwrap().modified().unwrap();
	fs::File::options().write(true).open(&a).unwrap().set_modified(t).unwrap();
	assert!(undo(&s).is_err());
	assert_eq!(fs::read_to_string(&b).unwrap(), "real numbers");
}

#[test]
fn long_histories_are_read_in_linear_time() {
	// Timed at two sizes in one process (the best of 5 reads each); no absolute limit.
	fn time(n: usize) -> std::time::Duration {
		let s = Sandbox::new();
		let _ = s.journal();
		let mut text = String::new();
		let mut line = |e: &Entry| {
			let json = serde_json::to_string(e).unwrap();
			text.push_str(&format!("{:08x} {json}\n", crc32fast::hash(json.as_bytes())));
		};
		// An interrupted batch first: nothing after it may be archived, so the journal is read
		// in full (compaction would otherwise keep it small).
		line(&Entry::Begin {
			batch: "open".into(),
			at: 0,
			source: "human".into(),
			reason: String::new(),
			digest: String::new(),
			v: 1,
		});
		for i in 0..n {
			let batch = format!("b{i}");
			let p = s.root.join(format!("d{i}"));
			line(&Entry::Begin {
				batch: batch.clone(),
				at: 0,
				source: "human".into(),
				reason: String::new(),
				digest: String::new(),
				v: 1,
			});
			line(&Entry::Intent {
				batch: batch.clone(),
				index: 0,
				forward: Op::Mkdir { path: p.clone() },
				inverse: Op::Rmdir { path: p },
			});
			line(&Entry::Done { batch: batch.clone(), index: 0, check: None, mode: None });
			line(&Entry::Commit { batch });
		}
		fs::write(s.state.join("journal.jsonl"), text).unwrap();
		let mut j = s.journal();
		(0..5)
			.map(|_| {
				let started = std::time::Instant::now();
				assert_eq!(j.batches().unwrap().len(), n + 1);
				started.elapsed()
			})
			.min()
			.unwrap()
	}
	// 16× the batches: about 16× the time if linear, 256× if quadratic; 64× leaves room for a
	// loaded machine either way.
	let (small, large) = (time(2_000), time(32_000));
	assert!(large < small * 64, "2k batches: {small:?}, 32k: {large:?}");
}

#[test]
fn an_old_unrecoverable_batch_can_be_abandoned_and_does_not_block_new_work() {
	let s = Sandbox::new();
	let mut j = s.journal();
	j.append(&Entry::Begin {
		batch: "old".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	j.append(&Entry::Begin {
		batch: "new".into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	j.append(&Entry::Commit { batch: "new".into() }).unwrap();
	drop(j);
	// The interrupted batch is old, with newer committed work on top: new work may go on.
	run(&s, vec![Op::Mkdir { path: s.root.join("d") }]);
	let mut j = s.journal();
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(report.errors[0].contains("old"), "{report:?}");
	Executor::new(&s.validator(), &mut j, s.trash()).abandon("old").unwrap();
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.is_empty(), "{report:?}");
	assert!(Executor::new(&s.validator(), &mut j, s.trash()).abandon("new").is_err());
}

/// A step whose own call fails changed nothing: what another program did meanwhile is left exactly
/// as it is, even when it looks like the step's result.
#[test]
fn a_failed_step_never_reverts_what_someone_else_did() {
	// move a -> b: another program removes a and writes its own b right before the rename.
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let b = s.root.join("b.txt");
	let (ra, rb) = (a.clone(), b.clone());
	let v = s.validator();
	let mut j = s.journal();
	let err = Executor::new(&v, &mut j, s.trash())
		.before_apply(move |_| {
			fs::remove_file(&ra).unwrap();
			fs::write(&rb, "OTHER PROGRAM").unwrap();
		})
		.execute(&Plan::new(vec![Op::Move { from: a.clone(), to: b.clone() }], ""), Source::Human)
		.unwrap_err();
	assert!(matches!(err, ExecError::Failed { step: 0, .. }), "{err}");
	assert_eq!(fs::read_to_string(&b).unwrap(), "OTHER PROGRAM");
	assert!(!a.exists(), "the other program's file was moved back to the old name");
	assert_eq!(j.batches().unwrap()[0].status, BatchStatus::Aborted);
	drop(j);

	// mkdir x: another program creates x first.
	let x = s.root.join("x");
	let rx = x.clone();
	let mut j = s.journal();
	let err = Executor::new(&v, &mut j, s.trash())
		.before_apply(move |_| {
			fs::create_dir(&rx).unwrap();
			fs::write(rx.join("theirs"), "T").unwrap();
		})
		.execute(&Plan::new(vec![Op::Mkdir { path: x.clone() }], ""), Source::Human)
		.unwrap_err();
	assert!(matches!(err, ExecError::Failed { step: 0, .. }), "{err}");
	assert_eq!(fs::read_to_string(x.join("theirs")).unwrap(), "T");
	// Recovering afterwards does not judge the failed step from the disk either.
	let report = Executor::new(&v, &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.is_empty() && report.rolled_back.is_empty(), "{report:?}");
	assert!(x.join("theirs").exists());
}

/// A move that crashed after its rename, with the old name taken again before recovery: the disk
/// cannot tell what happened, so recovery reports it and keeps the batch incomplete.
#[test]
fn recovery_does_not_claim_a_rollback_when_both_names_exist() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let b = s.root.join("b.txt");
	let mut j = s.journal();
	let batch = "1-crash".to_owned();
	j.append(&Entry::Begin {
		batch: batch.clone(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	j.append(&Entry::Intent {
		batch: batch.clone(),
		index: 0,
		forward: Op::Move { from: a.clone(), to: b.clone() },
		inverse: Op::Move { from: b.clone(), to: a.clone() },
	})
	.unwrap();
	fs::rename(&a, &b).unwrap();
	fs::write(&a, "NEW A").unwrap(); // the user saves a new a.txt before recovering

	let v = s.validator();
	let report = Executor::new(&v, &mut j, s.trash()).recover().unwrap();
	assert!(report.rolled_back.is_empty(), "{report:?}");
	assert_eq!(report.errors.len(), 1, "{report:?}");
	assert_eq!(j.batches().unwrap()[0].status, BatchStatus::Incomplete);
	assert_eq!(fs::read_to_string(&a).unwrap(), "NEW A");
	assert_eq!(fs::read_to_string(&b).unwrap(), "A");
	// The user decides: abandon keeps both and unblocks new work.
	Executor::new(&v, &mut j, s.trash()).abandon(&batch).unwrap();
	run_ok(&s, &mut j, vec![Op::Mkdir { path: s.root.join("later") }]);
}

fn run_ok(s: &Sandbox, j: &mut Journal, ops: Vec<Op>) {
	Executor::new(&s.validator(), j, s.trash()).execute(&Plan::new(ops, ""), Source::Human).unwrap();
}

/// Case-only renames in a big folder are validated without reading the folder once per rename.
#[test]
fn case_only_renames_in_a_big_folder_validate_quickly() {
	let s = Sandbox::new();
	let d = s.dir("big");
	for i in 0..20_000 {
		fs::write(d.join(format!("IMG_{i}.JPG")), "").unwrap();
	}
	let ops: Vec<Op> = (0..2_000)
		.map(|i| Op::Rename { path: d.join(format!("IMG_{i}.JPG")), new_name: format!("img_{i}.jpg") })
		.collect();
	let started = std::time::Instant::now();
	let issues = s.validator().validate(&Plan::new(ops, ""));
	assert!(issues.is_empty(), "{:?}", issues.first());
	assert!(started.elapsed() < std::time::Duration::from_secs(20), "{:?}", started.elapsed());
}

/// Recovery records each step it rolled back: interrupted after the newest step's inverse, it
/// resumes with the next one instead of judging the disk again (two steps here share a path).
#[test]
fn interrupted_recovery_resumes_after_the_steps_it_finished() {
	let s = Sandbox::new();
	let a = s.root.join("a.txt");
	let b = s.root.join("b.txt");
	fs::write(&b, "ORIGINAL").unwrap(); // after step 0 (move a -> b)
	fs::write(&a, "ORIGINAL").unwrap(); // after step 1 (copy b -> a); then recovery trashed a…
	let mut j = s.journal();
	let batch = "1-reuse".to_owned();
	j.append(&Entry::Begin {
		batch: batch.clone(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	for (i, forward, inverse) in [
		(0, Op::Move { from: a.clone(), to: b.clone() }, Op::Move { from: b.clone(), to: a.clone() }),
		(1, Op::Copy { from: b.clone(), to: a.clone() }, Op::Trash { path: a.clone() }),
	] {
		j.append(&Entry::Intent { batch: batch.clone(), index: i, forward, inverse }).unwrap();
		j.append(&Entry::Done { batch: batch.clone(), index: i, check: None, mode: None }).unwrap();
	}
	// …the first recovery trashed the copy at a, recorded it, and was cut off before step 0.
	fs::remove_file(&a).unwrap();
	j.append(&Entry::UndoStep { batch: batch.clone(), index: 1 }).unwrap();

	let v = s.validator();
	let report = Executor::new(&v, &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.is_empty(), "{:?}", report.errors);
	assert_eq!(fs::read_to_string(&a).unwrap(), "ORIGINAL", "the original is back at its place");
	assert!(!b.exists());
	assert_eq!(j.batches().unwrap()[0].status, BatchStatus::Aborted);
}

fn begin(j: &mut Journal, batch: &str) {
	j.append(&Entry::Begin {
		batch: batch.into(),
		at: 0,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
}

fn corrupt_line(s: &Sandbox) {
	let path = s.state.join("journal.jsonl");
	let mut bytes = fs::read(&path).unwrap();
	bytes.extend_from_slice(b"0000 {\"kind\":\"undo_step\"\n");
	fs::write(&path, bytes).unwrap();
}

/// A torn tail, then a recovery that rolls back one step, records it and stops on a conflict: once
/// the user clears the conflict, a second `recover` finishes (its own records are not "damage").
#[test]
fn recovery_after_a_torn_tail_may_take_several_runs() {
	let s = Sandbox::new();
	let (a, a2, b, b2) = (s.root.join("a"), s.root.join("a2"), s.root.join("b"), s.root.join("b2"));
	fs::write(&a2, "A").unwrap();
	fs::write(&b2, "B").unwrap();
	let mut j = s.journal();
	begin(&mut j, "x");
	for (i, from, to) in [(0, &a, &a2), (1, &b, &b2)] {
		j.append(&Entry::Intent {
			batch: "x".into(),
			index: i,
			forward: Op::Move { from: from.clone(), to: to.clone() },
			inverse: Op::Move { from: to.clone(), to: from.clone() },
		})
		.unwrap();
		j.append(&Entry::Done { batch: "x".into(), index: i, check: None, mode: None }).unwrap();
	}
	drop(j);
	corrupt_line(&s); // torn while writing
	fs::write(&a, "USER NEW").unwrap();
	let mut j = s.journal();
	let first = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert_eq!(first.errors.len(), 1, "{first:?}");
	assert_eq!(fs::read_to_string(&b).unwrap(), "B", "step 1 was rolled back");
	fs::remove_file(&a).unwrap();
	let second = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(second.errors.is_empty() && second.rolled_back == ["x"], "{second:?}");
	assert_eq!(fs::read_to_string(&a).unwrap(), "A");
}

/// A record lost in the middle of an undo of a committed batch: the batch is damaged, and undo
/// refuses it instead of replaying the lost step from what the disk looks like.
#[test]
fn a_record_lost_during_an_undo_makes_the_batch_damaged() {
	let s = Sandbox::new();
	fs::write(s.root.join("a"), "A").unwrap();
	let id = run(&s, vec![Op::Mkdir { path: s.root.join("d") }, Op::Mkdir { path: s.root.join("e") }]);
	let mut j = s.journal();
	j.append(&Entry::UndoStep { batch: id.clone(), index: 1 }).unwrap();
	drop(j);
	corrupt_line(&s); // the next UndoStep rotted
	let mut j = s.journal();
	j.append(&Entry::UndoStep { batch: id.clone(), index: 0 }).unwrap();
	let b = j.batches().unwrap().pop().unwrap();
	assert!(b.damaged, "a lost undo record went unnoticed");
	drop(j);
	assert!(undo(&s).is_err());
}

/// Recovery cut off after step 0's inverse ran but before it was recorded, with both steps on the
/// same path: the recorded step 1 is not replayed (that would trash the restored original).
#[test]
fn recovery_never_replays_a_step_it_recorded() {
	let s = Sandbox::new();
	let (a, b) = (s.root.join("a.txt"), s.root.join("b.txt"));
	fs::write(&a, "ORIGINAL").unwrap(); // step 0's inverse (b -> a) already ran
	let mut j = s.journal();
	begin(&mut j, "y");
	for (i, forward, inverse) in [
		(0, Op::Move { from: a.clone(), to: b.clone() }, Op::Move { from: b.clone(), to: a.clone() }),
		(1, Op::Copy { from: b.clone(), to: a.clone() }, Op::Trash { path: a.clone() }),
	] {
		j.append(&Entry::Intent { batch: "y".into(), index: i, forward, inverse }).unwrap();
		j.append(&Entry::Done { batch: "y".into(), index: i, check: None, mode: None }).unwrap();
	}
	j.append(&Entry::UndoStep { batch: "y".into(), index: 1 }).unwrap();
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.is_empty(), "{report:?}");
	assert_eq!(fs::read_to_string(&a).unwrap(), "ORIGINAL", "the restored original was trashed");
	assert!(!s.trash().exists() || fs::read_dir(s.trash()).unwrap().next().is_none());
}

/// When a step's inverse fails, recovery stops there: older inverses assume it ran.
#[test]
fn recovery_stops_at_a_step_it_cannot_roll_back() {
	let s = Sandbox::new();
	let (a, b, c) = (s.root.join("a"), s.root.join("b"), s.root.join("c"));
	fs::write(&c, "ORIGINAL").unwrap(); // after move a->b, move b->c
	let mut j = s.journal();
	begin(&mut j, "z");
	for (i, from, to) in [(0, &a, &b), (1, &b, &c)] {
		j.append(&Entry::Intent {
			batch: "z".into(),
			index: i,
			forward: Op::Move { from: from.clone(), to: to.clone() },
			inverse: Op::Move { from: to.clone(), to: from.clone() },
		})
		.unwrap();
		j.append(&Entry::Done { batch: "z".into(), index: i, check: None, mode: None }).unwrap();
	}
	fs::write(&b, "USER").unwrap(); // someone else creates b before recovery
	let first = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert_eq!(first.errors.len(), 1, "{first:?}");
	assert!(!a.exists(), "an older step was rolled back over the conflict");
	assert_eq!(fs::read_to_string(&b).unwrap(), "USER");
	fs::rename(&b, s.root.join("user-b")).unwrap();
	let second = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(second.errors.is_empty(), "{second:?}");
	assert_eq!(fs::read_to_string(&a).unwrap(), "ORIGINAL");
	assert_eq!(fs::read_to_string(s.root.join("user-b")).unwrap(), "USER");
}

/// A record lost while undoing an older batch (the newest one was undone already) is noticed.
#[test]
fn a_record_lost_while_undoing_an_older_batch_is_noticed() {
	let s = Sandbox::new();
	let older = run(&s, vec![Op::Mkdir { path: s.root.join("d") }, Op::Mkdir { path: s.root.join("e") }]);
	run(&s, vec![Op::Mkdir { path: s.root.join("n") }]);
	undo(&s).unwrap(); // the newer batch is undone
	let mut j = s.journal();
	j.append(&Entry::UndoStep { batch: older.clone(), index: 1 }).unwrap();
	drop(j);
	corrupt_line(&s);
	let mut j = s.journal();
	j.append(&Entry::UndoStep { batch: older.clone(), index: 0 }).unwrap();
	let b = j.batches().unwrap().into_iter().find(|b| b.id == older).unwrap();
	assert!(b.damaged, "the lost undo record of the older batch went unnoticed");
}

/// A copy whose publishing rename was refused (someone else created the target): the step is
/// recorded as failed before its part is removed, so a crash in between never makes recovery
/// take the other program's file for tpfs's copy.
#[test]
fn a_refused_publish_is_recorded_before_its_part_goes() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let b = s.root.join("b.txt");
	let rb = b.clone();
	let v = s.validator();
	let mut j = s.journal();
	let err = Executor::new(&v, &mut j, s.trash())
		.before_apply(move |_| fs::write(&rb, "THEIRS").unwrap())
		.execute(&Plan::new(vec![Op::Copy { from: a, to: b.clone() }], ""), Source::Human)
		.unwrap_err();
	assert!(matches!(err, ExecError::Failed { step: 0, .. }), "{err}");
	let batch = j.batches().unwrap().pop().unwrap();
	assert!(batch.steps[0].failed, "the failed step is recorded");
	// Recovering again (as after a crash) leaves the other program's file alone.
	let report = Executor::new(&v, &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.is_empty(), "{report:?}");
	assert_eq!(fs::read_to_string(&b).unwrap(), "THEIRS");
}

/// An interrupted step that cannot be cleaned up (a conflict) stops recovery: the older step's
/// inverse would otherwise run on top of it and move someone else's file.
#[test]
fn recovery_stops_at_an_interrupted_step_it_cannot_clean_up() {
	let s = Sandbox::new();
	let (x, a, b) = (s.root.join("x"), s.root.join("a"), s.root.join("b"));
	fs::write(&b, "MOVED").unwrap(); // x -> a (done), a -> b (renamed, not recorded)
	let mut j = s.journal();
	begin(&mut j, "w");
	j.append(&Entry::Intent {
		batch: "w".into(),
		index: 0,
		forward: Op::Move { from: x.clone(), to: a.clone() },
		inverse: Op::Move { from: a.clone(), to: x.clone() },
	})
	.unwrap();
	j.append(&Entry::Done { batch: "w".into(), index: 0, check: None, mode: None }).unwrap();
	j.append(&Entry::Intent {
		batch: "w".into(),
		index: 1,
		forward: Op::Move { from: a.clone(), to: b.clone() },
		inverse: Op::Move { from: b.clone(), to: a.clone() },
	})
	.unwrap();
	fs::write(&a, "USER").unwrap(); // someone reuses the name
	let first = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert_eq!(first.errors.len(), 1, "{first:?}");
	assert!(!x.exists(), "the older step moved someone else's file");
	assert_eq!(fs::read_to_string(&a).unwrap(), "USER");
	assert_eq!(fs::read_to_string(&b).unwrap(), "MOVED");
}

/// The first undo record of an older batch lost right after the newer batch's own `undo`.
#[test]
fn a_lost_first_undo_record_of_an_older_batch_is_noticed() {
	let s = Sandbox::new();
	let older = run(&s, vec![Op::Mkdir { path: s.root.join("d") }, Op::Rmdir { path: s.root.join("d") }]);
	run(&s, vec![Op::Mkdir { path: s.root.join("n") }]);
	undo(&s).unwrap();
	corrupt_line(&s); // UndoStep(older, 1) rotted
	let mut j = s.journal();
	j.append(&Entry::UndoStep { batch: older.clone(), index: 0 }).unwrap();
	let b = j.batches().unwrap().into_iter().find(|b| b.id == older).unwrap();
	assert!(b.damaged, "the lost first undo record went unnoticed");
	drop(j);
	assert!(undo(&s).is_err());
}

/// Undo of an `rmdir` when someone made a folder of that name since: its permissions and
/// contents stay as they are (undo only restores the mode of a folder it recreated itself).
#[cfg(unix)]
#[test]
fn undoing_rmdir_leaves_a_folder_made_since_alone() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	let d = s.root.join("shared");
	fs::create_dir(&d).unwrap();
	fs::set_permissions(&d, fs::Permissions::from_mode(0o777)).unwrap();
	run(&s, vec![Op::Rmdir { path: d.clone() }]);
	fs::create_dir(&d).unwrap();
	fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
	fs::write(d.join("secret"), "S").unwrap();
	assert!(undo(&s).is_err(), "undo went ahead over a folder made since");
	assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700, "the user's folder was opened up");
	assert_eq!(fs::read_to_string(d.join("secret")).unwrap(), "S");
}

/// Undo of an rmdir interrupted after its mkdir (journaled) and before the mode was set: the
/// rerun recognises the folder as its own, sets the mode and finishes.
#[cfg(unix)]
#[test]
fn an_interrupted_undo_of_rmdir_finishes_the_mode() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	let d = s.root.join("open");
	fs::create_dir(&d).unwrap();
	fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
	let id = run(&s, vec![Op::Rmdir { path: d.clone() }]);
	let mut j = s.journal();
	j.append(&Entry::UndoPublished { batch: id, index: 0 }).unwrap(); // journaled first…
	drop(j);
	fs::create_dir(&d).unwrap(); // …then made owner-only; the crash came before the mode was set
	fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
	assert_eq!(undo(&s).unwrap().unwrap().steps, 1);
	assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o755);
}

/// Linux: a mount point cannot be trashed or moved (it is refused at validation).
#[cfg(target_os = "linux")]
#[test]
fn a_mount_point_is_refused_for_trash_and_move() {
	use std::os::unix::fs::MetadataExt;
	let shm = Path::new("/dev/shm");
	if !shm.is_dir() || fs::metadata(shm).unwrap().dev() == fs::metadata("/dev").unwrap().dev() {
		return; // no separate file system there
	}
	let v = Validator::new(["/dev"]).unwrap();
	eprintln!("checking mount point refusal on /dev/shm");
	for op in [Op::Trash { path: shm.into() }, Op::Move { from: shm.into(), to: "/dev/shm-moved".into() }] {
		let issues = v.validate(&Plan::new(vec![op.clone()], ""));
		assert!(issues.iter().any(|i| i.message.contains("mount point")), "{op}: {issues:?}");
	}
}

/// On every OS: undo of an `rmdir` stops when a folder with content is at the name again.
#[test]
fn undoing_rmdir_stops_over_a_filled_folder_on_every_os() {
	let s = Sandbox::new();
	let d = s.root.join("d");
	fs::create_dir(&d).unwrap();
	let id = run(&s, vec![Op::Rmdir { path: d.clone() }]);
	fs::create_dir(&d).unwrap();
	fs::write(d.join("secret"), "S").unwrap();
	assert!(undo(&s).is_err());
	assert_eq!(fs::read_to_string(d.join("secret")).unwrap(), "S");
	let mut j = s.journal();
	assert_eq!(j.batches().unwrap().into_iter().find(|b| b.id == id).unwrap().status, BatchStatus::Committed);
}

/// A forged journal step whose inverse renames to a path, not a name, is refused by undo.
#[test]
fn a_forged_rename_to_a_path_is_refused() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	let mut j = s.journal();
	begin(&mut j, "f");
	j.append(&Entry::Intent {
		batch: "f".into(),
		index: 0,
		forward: Op::Rename { path: s.root.join("x"), new_name: "a.txt".into() },
		inverse: Op::Rename { path: a.clone(), new_name: "../escaped.txt".into() },
	})
	.unwrap();
	j.append(&Entry::Done { batch: "f".into(), index: 0, check: None, mode: None }).unwrap();
	j.append(&Entry::Commit { batch: "f".into() }).unwrap();
	drop(j);
	assert!(undo(&s).is_err());
	assert!(a.exists());
	assert!(!s.root.parent().unwrap().join("escaped.txt").exists());
}

/// tpfs's own work names are recognised in any letter case (case-insensitive volumes).
#[test]
fn own_names_are_recognised_in_any_case() {
	for n in [".tpfs-trash-1000", ".TPFS-TRASH-1000", ".Tpfs-Part-0123abcd-0", ".TPFS-CASE-DEADBEEF-1"] {
		assert!(tpfs_core::is_own_name(n.as_ref()), "{n}");
	}
	assert!(!tpfs_core::is_own_name(".TPFS-notes.txt".as_ref()));
	let s = Sandbox::new();
	let v = s.validator();
	let issues = v.validate(&Plan::new(vec![Op::Mkdir { path: s.root.join(".TPFS-TRASH-1000") }], ""));
	assert!(!issues.is_empty(), "a plan reached tpfs's own work name in another case");
}

/// A forged journal rename to a path that stays inside the root is refused too (the journaled
/// new name must be a plain name).
#[test]
fn a_forged_rename_into_a_subfolder_is_refused() {
	let s = Sandbox::new();
	let a = s.file("a.txt", "A");
	s.dir("sub");
	let mut j = s.journal();
	begin(&mut j, "g");
	j.append(&Entry::Intent {
		batch: "g".into(),
		index: 0,
		forward: Op::Rename { path: s.root.join("x"), new_name: "a.txt".into() },
		inverse: Op::Rename { path: a.clone(), new_name: "sub/moved.txt".into() },
	})
	.unwrap();
	j.append(&Entry::Done { batch: "g".into(), index: 0, check: None, mode: None }).unwrap();
	j.append(&Entry::Commit { batch: "g".into() }).unwrap();
	drop(j);
	assert!(undo(&s).is_err());
	assert!(a.exists() && !s.root.join("sub/moved.txt").exists());
}

/// Recovery finishes the mode of a folder an earlier, interrupted recovery recreated.
#[cfg(unix)]
#[test]
fn an_interrupted_recovery_of_rmdir_finishes_the_mode() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	let d = s.root.join("private");
	let mut j = s.journal();
	begin(&mut j, "h");
	j.append(&Entry::Intent {
		batch: "h".into(),
		index: 0,
		forward: Op::Rmdir { path: d.clone() },
		inverse: Op::Mkdir { path: d.clone() },
	})
	.unwrap();
	j.append(&Entry::Mode { batch: "h".into(), index: 0, mode: 0o755 }).unwrap();
	// The rmdir ran; then a recovery recorded the folder, made it owner-only and was cut off.
	j.append(&Entry::UndoPublished { batch: "h".into(), index: 0 }).unwrap();
	fs::create_dir(&d).unwrap();
	fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
	let report = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(report.errors.is_empty(), "{report:?}");
	assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o755);
	let again = Executor::new(&s.validator(), &mut j, s.trash()).recover().unwrap();
	assert!(again.errors.is_empty() && again.rolled_back.is_empty(), "{again:?}");
}

/// Undo of an `rmdir` never opens up a private folder the user made at that name since.
#[cfg(unix)]
#[test]
fn undoing_rmdir_never_widens_an_empty_folder_made_since() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	let d = s.root.join("shared");
	fs::create_dir(&d).unwrap();
	fs::set_permissions(&d, fs::Permissions::from_mode(0o777)).unwrap();
	run(&s, vec![Op::Rmdir { path: d.clone() }]);
	fs::create_dir(&d).unwrap();
	fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
	assert!(undo(&s).is_err());
	assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
}

/// Undo of an `rmdir` of a folder wider than owner-only brings back its full mode.
#[cfg(unix)]
#[test]
fn undoing_rmdir_restores_a_wide_mode() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	let d = s.root.join("wide");
	fs::create_dir(&d).unwrap();
	fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
	run(&s, vec![Op::Rmdir { path: d.clone() }]);
	assert_eq!(undo(&s).unwrap().unwrap().steps, 1);
	assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o755);
}

/// Undo of a named batch never undoes another one committed since it was shown.
#[test]
fn undo_batch_refuses_when_a_newer_batch_was_committed() {
	let s = Sandbox::new();
	let a = run(&s, vec![Op::Mkdir { path: s.root.join("a") }]);
	let b = run(&s, vec![Op::Mkdir { path: s.root.join("b") }]);
	let mut j = s.journal();
	let err = Executor::new(&s.validator(), &mut j, s.trash()).undo_batch(&a).unwrap_err();
	assert!(err.to_string().contains("no longer the newest batch"), "{err}");
	assert!(s.root.join("a").exists() && s.root.join("b").exists());
	let done = Executor::new(&s.validator(), &mut j, s.trash()).undo_batch(&b).unwrap().unwrap();
	assert_eq!(done.id, b);
	assert!(!s.root.join("b").exists() && s.root.join("a").exists());
}

/// Undo of an `rmdir` brings back a folder whose mode does not let its owner read it (such a
/// folder could be removed by root, or by a plan made before its mode changed).
#[cfg(unix)]
#[test]
fn undoing_rmdir_restores_a_mode_without_read() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	for mode in [0o300, 0o000] {
		let d = s.root.join(format!("m{mode:o}"));
		let batch = format!("r{mode:o}");
		let mut j = s.journal();
		begin(&mut j, &batch);
		for e in [
			Entry::Intent {
				batch: batch.clone(),
				index: 0,
				forward: Op::Rmdir { path: d.clone() },
				inverse: Op::Mkdir { path: d.clone() },
			},
			Entry::Mode { batch: batch.clone(), index: 0, mode },
			Entry::Done { batch: batch.clone(), index: 0, check: None, mode: Some(mode) },
			Entry::Commit { batch: batch.clone() },
		] {
			j.append(&e).unwrap();
		}
		drop(j);
		assert_eq!(undo(&s).unwrap().unwrap().steps, 1);
		assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, mode);
		fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
	}
}

/// Paths deeper than the limit are refused up front (so validation stays quick); at the limit
/// they are accepted.
#[test]
fn very_deep_paths_are_refused() {
	let s = Sandbox::new();
	let depth = s.root.components().count();
	let at = |n: usize| {
		let mut p = s.root.clone();
		for _ in depth..n {
			p.push("a");
		}
		Plan::new(vec![Op::Mkdir { path: p }], "")
	};
	let started = std::time::Instant::now();
	let deep = messages(&s.validator(), &at(tpfs_core::MAX_DEPTH + 1));
	assert!(deep.iter().any(|m| m.contains("folders deep")), "{deep:?}");
	assert!(started.elapsed().as_secs() < 5);
	let ok = messages(&s.validator(), &at(tpfs_core::MAX_DEPTH));
	assert!(!ok.iter().any(|m| m.contains("folders deep")), "{ok:?}");
}

/// Every character `fold` maps to plain letters, and the invisible ones it drops, still name
/// tpfs's own work folders and its state.
#[test]
fn folded_spellings_of_own_names_and_state_are_recognised() {
	for n in [
		".tpf\u{17f}-trash-1000",
		".tpfs-tra\u{17f}h-1000",
		".TPFS-TRASH-1000",
		".tpfs-tra\u{fb06}-1000", // the "st" ligature: ".tpfs-trast-1000", not an own name
		".tpfs\u{200b}-trash-1000",
		".tpfs\u{200c}-trash-1000",
		".tpfs-trash\u{feff}-1000",
		".tpfs-tra\u{ad}sh-1000",
		".tpfs-case-0123abcd-0\u{200d}",
	] {
		let own = tpfs_core::is_own_name(n.as_ref());
		assert_eq!(own, !n.contains('\u{fb06}'), "{n:?}");
	}
	// Ligatures fold to their letters: a state folder named with them is matched by them.
	let s = Sandbox::new();
	let base = s.root.parent().unwrap().to_path_buf();
	for (name, spelled) in [
		("stuff", "\u{fb06}uff"),
		("office", "o\u{fb03}ce"),
		("kelvin", "\u{212a}elvin"),
		("fl", "\u{fb02}"),
		("ff", "\u{fb00}"),
		("fi", "\u{fb01}"),
		("ffl", "\u{fb04}"),
	] {
		let v = Validator::new([&base]).unwrap().with_protected([base.join(name)]);
		let plan = Plan::new(vec![Op::Mkdir { path: base.join(spelled).join("x") }], "");
		assert!(messages(&v, &plan).iter().any(|m| m.contains("tpfs's state")), "{spelled}: {:?}", messages(&v, &plan));
	}
}

/// A copy whose result would be deeper than tpfs can address is refused before it starts, so a
/// copy can never leave a partial tree tpfs cannot remove again.
#[test]
fn a_copy_too_deep_for_its_target_is_refused() {
	let s = Sandbox::new();
	let mut src = s.root.join("src");
	for _ in 0..200 {
		src.push("d");
	}
	fs::create_dir_all(&src).unwrap();
	let mut target = s.root.clone();
	for _ in 0..60 {
		target.push("t");
	}
	fs::create_dir_all(&target).unwrap();
	let plan = Plan::new(vec![Op::Copy { from: s.root.join("src"), to: target.join("copy") }], "");
	let m = messages(&s.validator(), &plan);
	assert!(m.iter().any(|m| m.contains("Too deep to copy")), "{m:?}");
	// A shallow target takes the same tree.
	let plan = Plan::new(vec![Op::Copy { from: s.root.join("src"), to: s.root.join("copy") }], "");
	assert!(messages(&s.validator(), &plan).is_empty(), "{:?}", messages(&s.validator(), &plan));
	// …and copying it there first does not let a second copy to the deep target through.
	let plan = Plan::new(
		vec![
			Op::Copy { from: s.root.join("src"), to: s.root.join("copy") },
			Op::Copy { from: s.root.join("src"), to: target.join("copy") },
		],
		"",
	);
	let m = messages(&s.validator(), &plan);
	assert!(m.iter().any(|m| m.starts_with("#2") && m.contains("Too deep to copy")), "{m:?}");
}

/// Errors name a batch only in printable form, whatever the journal holds.
#[test]
fn undo_errors_print_batch_ids_safely() {
	let e = ExecError::Undo { batch: "evil\u{1b}]52;c;x\u{7}".into(), error: "x".into() };
	assert!(!e.to_string().chars().any(char::is_control), "{e}");
}

/// Journal text of `n` committed one-step batches named `c<i>` (each a mkdir under `root`).
fn committed_batches(root: &Path, n: usize) -> String {
	let mut text = String::new();
	let mut line = |e: &Entry| {
		let json = serde_json::to_string(e).unwrap();
		text.push_str(&format!("{:08x} {json}\n", crc32fast::hash(json.as_bytes())));
	};
	for i in 0..n {
		let batch = format!("c{i}");
		let p = root.join(format!("d{i}"));
		line(&Entry::Begin {
			batch: batch.clone(),
			at: 0,
			source: "human".into(),
			reason: String::new(),
			digest: String::new(),
			v: 1,
		});
		line(&Entry::Intent {
			batch: batch.clone(),
			index: 0,
			forward: Op::Mkdir { path: p.clone() },
			inverse: Op::Rmdir { path: p },
		});
		line(&Entry::Done { batch: batch.clone(), index: 0, check: None, mode: None });
		line(&Entry::Commit { batch });
	}
	text
}

/// Finished history moves to the archive: undone and rolled back batches go, committed ones stay
/// undoable, and `archived()` still shows what went.
#[test]
fn compaction_archives_finished_batches_and_keeps_undo_working() {
	let s = Sandbox::new();
	let a = run(&s, vec![Op::Mkdir { path: s.root.join("a") }]);
	let b = run(&s, vec![Op::Mkdir { path: s.root.join("b") }]);
	undo(&s).unwrap().unwrap(); // b undone
	let mut j = s.journal();
	begin(&mut j, "rolled-back");
	j.append(&Entry::Abort { batch: "rolled-back".into() }).unwrap();
	let before = fs::metadata(j.path()).unwrap().len();
	assert_eq!(j.compact().unwrap(), 2);
	assert!(fs::metadata(j.path()).unwrap().len() < before);
	let left = j.batches().unwrap();
	assert_eq!(left.iter().map(|b| b.id.as_str()).collect::<Vec<_>>(), [a.as_str()]);
	let archived = j.archived().unwrap();
	assert_eq!(archived.len(), 2);
	assert!(archived.iter().any(|x| x.id == b && x.status == BatchStatus::Undone));
	assert_eq!(j.compact().unwrap(), 0, "nothing more to archive");
	drop(j);
	assert_eq!(undo(&s).unwrap().unwrap().id, a);
	assert!(!s.root.join("a").exists());
	assert!(!s.state.join("journal.jsonl.compact").exists());
}

/// Only the newest committed batches stay undoable; the older ones are archived.
#[test]
fn compaction_keeps_the_newest_committed_batches() {
	let s = Sandbox::new();
	drop(s.journal());
	let n = tpfs_core::KEEP_COMMITTED + 5;
	fs::write(s.state.join("journal.jsonl"), committed_batches(&s.root, n)).unwrap();
	let mut j = s.journal();
	assert_eq!(j.compact().unwrap(), 5);
	let left = j.batches().unwrap();
	assert_eq!(left.len(), tpfs_core::KEEP_COMMITTED);
	assert_eq!(left[0].id, "c5");
	assert_eq!(left.last().unwrap().id, format!("c{}", n - 1));
	assert_eq!(j.archived().unwrap().iter().map(|b| b.id.clone()).collect::<Vec<_>>(), ["c0", "c1", "c2", "c3", "c4"]);
}

/// Nothing at or after an interrupted batch is archived (its recovery looks at what came after
/// it), and nothing at all while the journal has an unreadable line.
#[test]
fn compaction_leaves_interrupted_batches_and_damage_alone() {
	let s = Sandbox::new();
	let a = run(&s, vec![Op::Mkdir { path: s.root.join("a") }]);
	undo(&s).unwrap().unwrap();
	let mut j = s.journal();
	begin(&mut j, "open");
	drop(j);
	let b = run_unchecked(&s, vec![Op::Mkdir { path: s.root.join("b") }]);
	let mut j = s.journal();
	// `b` was refused (a batch is still interrupted), so the journal holds a, open.
	assert!(b.is_err());
	assert_eq!(j.compact().unwrap(), 1, "only the undone batch before the interrupted one");
	assert_eq!(j.archived().unwrap()[0].id, a);
	assert_eq!(j.batches().unwrap()[0].id, "open");

	let mut raw = fs::OpenOptions::new().append(true).open(j.path()).unwrap();
	std::io::Write::write_all(&mut raw, b"garbage line\n").unwrap();
	assert_eq!(j.compact().unwrap(), 0);
}

fn run_unchecked(s: &Sandbox, ops: Vec<Op>) -> Result<String, ExecError> {
	let mut j = s.journal();
	Executor::new(&s.validator(), &mut j, s.trash()).execute(&Plan::new(ops, ""), Source::Human).map(|b| b.id)
}

/// A big journal is compacted when it is opened, and the archive survives a compaction cut short
/// (records appended twice are shown once).
#[test]
fn a_big_journal_is_compacted_on_open() {
	let s = Sandbox::new();
	drop(s.journal());
	let n = tpfs_core::KEEP_COMMITTED + 20_000;
	let text = committed_batches(&s.root, n);
	assert!(text.len() as u64 >= 4 << 20, "{}", text.len());
	fs::write(s.state.join("journal.jsonl"), &text).unwrap();
	let mut j = s.journal();
	assert_eq!(j.batches().unwrap().len(), tpfs_core::KEEP_COMMITTED);
	let archive = j.archive_path();
	// A compaction cut short after it appended to the archive: the same records again.
	let again = fs::read(&archive).unwrap();
	let mut f = fs::OpenOptions::new().append(true).open(&archive).unwrap();
	std::io::Write::write_all(&mut f, &again).unwrap();
	assert_eq!(j.archived().unwrap().len(), 20_000);
}

/// One engine at a time, also across a compaction (the lock is on its own file).
#[test]
fn the_journal_lock_holds_across_compaction() {
	let s = Sandbox::new();
	run(&s, vec![Op::Mkdir { path: s.root.join("a") }]);
	undo(&s).unwrap().unwrap();
	let mut j = s.journal();
	assert_eq!(j.compact().unwrap(), 1);
	let second = Journal::open(s.state.join("journal.jsonl"));
	assert!(second.is_err_and(|e| e.to_string().contains("in use")));
}
