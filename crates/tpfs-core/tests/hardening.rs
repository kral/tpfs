//! v0.2 hardening: races, crashes at every step, disk space, trash per file system, hard links.
#![cfg_attr(not(target_os = "linux"), allow(unused_imports, dead_code))]

use std::{
	fs,
	panic::{self, AssertUnwindSafe},
	path::{Path, PathBuf},
};

use tempfile::TempDir;
use tpfs_core::{
	BatchStatus, ExecError, Executor, Journal, Op, Phase, Plan, Source, Validator,
	risk::{self, Level},
};

struct Sandbox {
	_tmp: TempDir,
	base: PathBuf,
	root: PathBuf,
	state: PathBuf,
}

impl Sandbox {
	fn new() -> Self {
		Self::new_in(None)
	}

	fn new_in(dir: Option<&Path>) -> Self {
		let tmp = match dir {
			Some(d) => TempDir::new_in(d).unwrap(),
			None => TempDir::new().unwrap(),
		};
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		let root = base.join("root");
		let state = base.join("state");
		fs::create_dir(&root).unwrap();
		Self { _tmp: tmp, base, root, state }
	}

	fn validator(&self) -> Validator {
		Validator::new([&self.root]).unwrap()
	}

	fn journal(&self) -> Journal {
		Journal::open(self.state.join("journal.jsonl")).unwrap()
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

/// A symlink swapped into the path after the step was checked must not redirect the operation.
#[cfg(target_os = "linux")]
#[test]
fn symlink_swapped_in_mid_plan_cannot_redirect_a_move() {
	let s = Sandbox::new();
	let outside = s.base.join("outside");
	fs::create_dir(&outside).unwrap();
	fs::write(outside.join("secret.txt"), "secret").unwrap();

	let dir = s.root.join("dir");
	fs::create_dir(&dir).unwrap();
	fs::write(dir.join("secret.txt"), "mine").unwrap();
	let plan = Plan::new(vec![Op::Move { from: dir.join("secret.txt"), to: s.root.join("grabbed.txt") }], "");

	let v = s.validator();
	let mut j = s.journal();
	let (dir2, outside2) = (dir.clone(), outside.clone());
	let err = Executor::new(&v, &mut j, s.state.join("trash"))
		.before_apply(move |_| {
			// The attacker replaces root/dir with a symlink to a folder outside the root.
			fs::rename(&dir2, dir2.with_file_name("dir-real")).unwrap();
			std::os::unix::fs::symlink(&outside2, &dir2).unwrap();
		})
		.execute(&plan, Source::Human)
		.unwrap_err();

	assert!(matches!(err, ExecError::Failed { .. }), "{err}");
	assert!(err.to_string().contains("symlink"), "{err}");
	assert_eq!(fs::read_to_string(outside.join("secret.txt")).unwrap(), "secret");
	assert!(!s.root.join("grabbed.txt").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn symlink_swapped_in_mid_plan_cannot_redirect_a_mkdir() {
	let s = Sandbox::new();
	let outside = s.base.join("outside");
	fs::create_dir(&outside).unwrap();
	let dir = s.root.join("dir");
	fs::create_dir(&dir).unwrap();
	let plan = Plan::new(vec![Op::Mkdir { path: dir.join("planted") }], "");

	let v = s.validator();
	let mut j = s.journal();
	let (dir2, outside2) = (dir.clone(), outside.clone());
	let err = Executor::new(&v, &mut j, s.state.join("trash"))
		.before_apply(move |_| {
			fs::remove_dir(&dir2).unwrap();
			std::os::unix::fs::symlink(&outside2, &dir2).unwrap();
		})
		.execute(&plan, Source::Human)
		.unwrap_err();

	assert!(matches!(err, ExecError::Failed { .. }), "{err}");
	assert!(!outside.join("planted").exists(), "mkdir followed the swapped-in symlink");
}

/// Crash before and after every step of a mixed plan; `recover` must always restore the original tree.
#[test]
fn crash_at_every_step_is_recovered() {
	let make = || {
		let s = Sandbox::new();
		fs::write(s.root.join("a.txt"), "A").unwrap();
		fs::write(s.root.join("b.txt"), "B").unwrap();
		fs::write(s.root.join("c.txt"), "C").unwrap();
		fs::create_dir(s.root.join("sub")).unwrap();
		fs::write(s.root.join("sub/inner.txt"), "I").unwrap();
		let r = &s.root;
		let plan = Plan::new(
			vec![
				Op::Mkdir { path: r.join("new") },
				Op::Move { from: r.join("a.txt"), to: r.join("new/a.txt") },
				Op::Copy { from: r.join("sub"), to: r.join("new/sub-copy") },
				Op::Rename { path: r.join("b.txt"), new_name: "renamed.txt".into() },
				Op::Trash { path: r.join("c.txt") },
				Op::Mkdir { path: r.join("empty") },
				Op::Rmdir { path: r.join("empty") },
			],
			"crash test",
		);
		(s, plan)
	};

	let steps = make().1.ops.len();
	for crash_at in 0..steps {
		for phase in [Phase::BeforeApply, Phase::AfterApply] {
			let (s, plan) = make();
			let before = s.snapshot();

			let default_hook = panic::take_hook();
			panic::set_hook(Box::new(|_| {}));
			let crashed = panic::catch_unwind(AssertUnwindSafe(|| {
				let v = s.validator();
				let mut j = s.journal();
				let _ = Executor::new(&v, &mut j, s.state.join("trash"))
					.hook(move |i, p| {
						if i == crash_at && p == phase {
							panic!("simulated crash");
						}
					})
					.execute(&plan, Source::Human);
			}));
			panic::set_hook(default_hook);
			assert!(crashed.is_err(), "no crash at step {crash_at} {phase:?}");

			let v = Validator::new(["/"]).unwrap();
			let mut j = s.journal();
			let report = Executor::new(&v, &mut j, s.state.join("trash")).recover().unwrap();
			assert!(report.errors.is_empty(), "step {crash_at} {phase:?}: {:?}", report.errors);
			assert_eq!(s.snapshot(), before, "state differs after a crash at step {crash_at} {phase:?}");
			assert_eq!(j.batches().unwrap()[0].status, BatchStatus::Aborted);
		}
	}
}

#[cfg(target_os = "linux")]
#[test]
fn refuses_a_copy_that_does_not_fit() {
	let s = Sandbox::new();
	let big = s.root.join("huge.img");
	// Sparse: logically 1 TiB, physically nothing.
	fs::File::create(&big).unwrap().set_len(1 << 40).unwrap();

	let plan = Plan::new(vec![Op::Copy { from: big, to: s.root.join("copy.img") }], "");
	let issues = s.validator().validate(&plan);
	assert!(issues.len() == 1 && issues[0].message.contains("Not enough space"), "{issues:?}");
}

/// Trashing on another file system than the global trash goes to a trash on that file system (a rename).
#[cfg(target_os = "linux")]
#[test]
fn trash_stays_on_the_same_file_system() {
	use std::os::unix::fs::MetadataExt;

	let shm = Path::new("/dev/shm");
	let state_tmp = TempDir::new().unwrap();
	if !shm.is_dir() || fs::metadata(shm).unwrap().dev() == fs::metadata(state_tmp.path()).unwrap().dev() {
		eprintln!("skipped: no second file system available");
		return;
	}

	let s = Sandbox::new_in(Some(shm));
	let file = s.root.join("note.txt");
	fs::write(&file, "N").unwrap();
	let trash = state_tmp.path().join("trash");

	let v = s.validator();
	let mut j = Journal::open(state_tmp.path().join("journal.jsonl")).unwrap();
	let batch = Executor::new(&v, &mut j, &trash)
		.execute(&Plan::new(vec![Op::Trash { path: file.clone() }], ""), Source::Human)
		.unwrap();

	let uid = fs::metadata(&s.root).unwrap().uid();
	let local = shm.join(format!(".tpfs-trash-{uid}")).join(&batch.id);
	assert!(local.is_dir(), "the file was not trashed on its own file system");
	assert!(!trash.join(&batch.id).exists());

	Executor::new(&Validator::new(["/"]).unwrap(), &mut j, &trash).undo_last().unwrap().unwrap();
	assert_eq!(fs::read_to_string(&file).unwrap(), "N");
}

#[cfg(unix)]
#[test]
fn trashing_a_hard_linked_file_is_flagged() {
	let s = Sandbox::new();
	let a = s.root.join("a.txt");
	fs::write(&a, "A").unwrap();
	fs::hard_link(&a, s.root.join("also-a.txt")).unwrap();

	let r = risk::assess(&Plan::new(vec![Op::Trash { path: a }], ""), Source::Human);
	assert_eq!(r.level, Level::Medium);
	assert!(r.reasons.iter().any(|x| x.contains("hard link")), "{:?}", r.reasons);
}
