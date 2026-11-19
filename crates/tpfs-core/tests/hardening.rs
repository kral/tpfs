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

			let v = Validator::new(tpfs_core::system_roots()).unwrap();
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

/// The per-file-system trash on /dev/shm, in one test (they share one folder name): planted ones
/// (readable by others, a symlink) are never used; the genuine one is used, and undo works.
#[cfg(target_os = "linux")]
#[test]
fn trash_stays_on_the_same_file_system_and_planted_trashes_are_refused() {
	use std::os::unix::fs::{MetadataExt, PermissionsExt};

	let shm = Path::new("/dev/shm");
	let state_tmp = TempDir::new().unwrap();
	if !shm.is_dir() || fs::metadata(shm).unwrap().dev() == fs::metadata(state_tmp.path()).unwrap().dev() {
		// Only a developer machine may lack it; CI must exercise this path.
		assert!(std::env::var_os("CI").is_none(), "CI needs /dev/shm on its own file system");
		eprintln!("skipped: no second file system available");
		return;
	}
	fs::set_permissions(state_tmp.path(), fs::Permissions::from_mode(0o700)).unwrap();
	// /dev/shm is shared by every test run on this host: one at a time (held until the end).
	let lock = fs::File::create(std::env::temp_dir().join(format!("tpfs-devshm-test-{}.lock", rustix_uid()))).unwrap();
	use std::os::fd::AsRawFd;
	// SAFETY: flock on a descriptor we own; it is released when `lock` is dropped.
	assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
	let uid = rustix_uid();
	let local = shm.join(format!(".tpfs-trash-{uid}"));
	// Keep whatever is there out of the way, and put it back at the end.
	let aside = shm.join(format!(".tpfs-trash-{uid}.aside-{}", std::process::id()));
	let had = fs::symlink_metadata(&local).is_ok();
	if had {
		fs::rename(&local, &aside).unwrap();
	}
	// Put things back however the test ends (a failed assertion included).
	struct Restore {
		local: PathBuf,
		aside: Option<PathBuf>,
	}
	impl Drop for Restore {
		fn drop(&mut self) {
			match fs::symlink_metadata(&self.local) {
				Ok(m) if m.is_dir() => {
					let _ = fs::remove_dir_all(&self.local);
				}
				Ok(_) => {
					let _ = fs::remove_file(&self.local);
				}
				Err(_) => {}
			}
			if let Some(a) = &self.aside {
				let _ = fs::rename(a, &self.local);
			}
		}
	}
	let _restore = Restore { local: local.clone(), aside: had.then(|| aside.clone()) };
	let trash = state_tmp.path().join("trash");
	let trash_one = |name: &str| {
		let s = Sandbox::new_in(Some(shm));
		let file = s.root.join(name);
		fs::write(&file, "N").unwrap();
		let mut j = Journal::open(state_tmp.path().join("journal.jsonl")).unwrap();
		let b = Executor::new(&s.validator(), &mut j, &trash)
			.execute(&Plan::new(vec![Op::Trash { path: file.clone() }], ""), Source::Human)
			.unwrap();
		(s, file, b.id, j)
	};

	// Readable by others: refused.
	fs::create_dir(&local).unwrap();
	fs::set_permissions(&local, fs::Permissions::from_mode(0o755)).unwrap();
	let (_s, _f, id, _) = trash_one("a.txt");
	assert!(!local.join(&id).exists() && trash.join(&id).exists(), "used a trash readable by others");
	fs::remove_dir(&local).unwrap();

	// A symlink to a folder the attacker controls: refused.
	let bait = TempDir::new_in(shm).unwrap();
	fs::set_permissions(bait.path(), fs::Permissions::from_mode(0o700)).unwrap();
	std::os::unix::fs::symlink(bait.path(), &local).unwrap();
	let (_s, _f, id, _) = trash_one("b.txt");
	assert!(fs::read_dir(bait.path()).unwrap().next().is_none(), "trashed into a planted symlink: {id}");
	fs::remove_file(&local).unwrap();

	// The genuine one: created private, used, and undo brings the file back.
	let (_s, file, id, mut j) = trash_one("c.txt");
	assert!(local.join(&id).is_dir(), "the file was not trashed on its own file system");
	assert!(!trash.join(&id).exists());
	Executor::new(&Validator::new(tpfs_core::system_roots()).unwrap(), &mut j, &trash).undo_last().unwrap().unwrap();
	assert_eq!(fs::read_to_string(&file).unwrap(), "N");

	// The genuine trash is a private folder owned by us.
	let m = fs::symlink_metadata(&local).unwrap();
	assert!(m.is_dir() && m.uid() == uid && m.permissions().mode() & 0o777 == 0o700);
}

#[cfg(target_os = "linux")]
fn rustix_uid() -> u32 {
	// SAFETY: getuid has no preconditions and cannot fail.
	unsafe { libc::getuid() }
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

/// A crash before a copy runs, then someone else creates the target: `recover` must not touch it.
#[test]
fn recover_leaves_a_target_that_is_not_ours() {
	let s = Sandbox::new();
	fs::write(s.root.join("a.txt"), "A").unwrap();
	let plan = Plan::new(vec![Op::Copy { from: s.root.join("a.txt"), to: s.root.join("b.txt") }], "");
	let foreign = s.root.join("b.txt");

	let default_hook = panic::take_hook();
	panic::set_hook(Box::new(|_| {}));
	let crashed = panic::catch_unwind(AssertUnwindSafe(|| {
		let v = s.validator();
		let mut j = s.journal();
		let _ = Executor::new(&v, &mut j, s.state.join("trash"))
			.hook(move |_, p| {
				if p == Phase::BeforeApply {
					panic!("simulated crash");
				}
			})
			.execute(&plan, Source::Human);
	}));
	panic::set_hook(default_hook);
	assert!(crashed.is_err());

	fs::write(&foreign, "someone else's").unwrap();
	let v = Validator::new(tpfs_core::system_roots()).unwrap();
	let mut j = s.journal();
	let report = Executor::new(&v, &mut j, s.state.join("trash")).recover().unwrap();
	assert!(report.errors.is_empty(), "{:?}", report.errors);
	assert_eq!(fs::read_to_string(&foreign).unwrap(), "someone else's");
}

/// Without spawning a process: a child forked by another test would briefly inherit open journal
/// descriptors and hold their lock.
#[cfg(unix)]
fn mkfifo(p: &Path) {
	use std::os::unix::ffi::OsStrExt;
	let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
	// SAFETY: `c` is a valid NUL-terminated path that outlives the call.
	assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

/// A regular file swapped for a FIFO after validation must not make the copy block forever,
/// and a FIFO inside a copied folder must not be read.
#[cfg(unix)]
#[test]
fn copies_never_read_through_fifos() {
	let s = Sandbox::new();
	let f = s.root.join("data.bin");
	fs::write(&f, "data").unwrap();
	let plan = Plan::new(vec![Op::Copy { from: f.clone(), to: s.root.join("copy.bin") }], "");
	let swap = f.clone();
	let v = s.validator();
	let mut j = s.journal();
	let err = Executor::new(&v, &mut j, s.state.join("trash"))
		.before_apply(move |_| {
			fs::remove_file(&swap).unwrap();
			mkfifo(&swap);
		})
		.execute(&plan, Source::Human)
		.unwrap_err();
	assert!(matches!(err, ExecError::Failed { .. }), "{err}");
	assert!(!s.root.join("copy.bin").exists());

	let dir = s.root.join("dir");
	fs::create_dir(&dir).unwrap();
	fs::write(dir.join("ok.txt"), "ok").unwrap();
	mkfifo(&dir.join("pipe"));
	let plan = Plan::new(vec![Op::Copy { from: dir.clone(), to: s.root.join("dir-copy") }], "");
	let err = Executor::new(&v, &mut j, s.state.join("trash")).execute(&plan, Source::Human).unwrap_err();
	assert!(err.to_string().contains("special file"), "{err}");
	assert!(!s.root.join("dir-copy").exists(), "the partial copy is removed");
}

/// Error messages carry file names; they must not carry terminal escapes with them.
#[cfg(unix)]
#[test]
fn errors_never_contain_raw_escape_sequences() {
	let s = Sandbox::new();
	let dir = s.root.join("dir");
	fs::create_dir(&dir).unwrap();
	mkfifo(&dir.join("x\u{1b}]52;c;aGk=\u{7}\u{1b}[2Jy"));
	let v = s.validator();
	let mut j = s.journal();
	let err = Executor::new(&v, &mut j, s.state.join("trash"))
		.execute(&Plan::new(vec![Op::Copy { from: dir, to: s.root.join("copy") }], ""), Source::Human)
		.unwrap_err()
		.to_string();
	assert!(!err.contains('\u{1b}') && !err.contains('\u{7}'), "{err:?}");
}

/// macOS and Windows: a folder swapped for a link right before a step runs is not followed.
#[cfg(any(target_os = "macos", windows))]
#[test]
fn a_link_swapped_in_before_a_step_is_not_followed() {
	let s = Sandbox::new();
	let sub = s.root.join("sub");
	fs::create_dir(&sub).unwrap();
	fs::write(s.root.join("f.txt"), "F").unwrap();
	let outside = s.base.join("outside");
	fs::create_dir(&outside).unwrap();
	let plan = Plan::new(vec![Op::Copy { from: s.root.join("f.txt"), to: sub.join("f.txt") }], "");
	let (sub2, out2) = (sub.clone(), outside.clone());
	let v = s.validator();
	let mut j = s.journal();
	let result = Executor::new(&v, &mut j, s.state.join("trash"))
		.before_apply(move |_| {
			fs::remove_dir(&sub2).unwrap();
			link_dir(&out2, &sub2);
		})
		.execute(&plan, Source::Human);
	assert!(result.is_err(), "the copy went through the link");
	assert!(!outside.join("f.txt").exists());
}

#[cfg(unix)]
fn link_dir(target: &Path, link: &Path) {
	std::os::unix::fs::symlink(target, link).unwrap();
}

/// A junction: no privilege needed, unlike a symlink.
#[cfg(windows)]
fn link_dir(target: &Path, link: &Path) {
	let ok = std::process::Command::new("cmd")
		.args(["/C", "mklink", "/J"])
		.arg(link)
		.arg(target)
		.stdout(std::process::Stdio::null())
		.status()
		.unwrap()
		.success();
	assert!(ok, "mklink /J failed");
}

#[cfg(windows)]
#[test]
fn copying_a_junction_is_refused_up_front() {
	let s = Sandbox::new();
	let target = s.root.join("target");
	fs::create_dir(&target).unwrap();
	let j = s.root.join("junction");
	link_dir(&target, &j);
	let issues = s.validator().validate(&Plan::new(vec![Op::Copy { from: j, to: s.root.join("copy") }], ""));
	assert!(issues.len() == 1 && issues[0].message.contains("not supported on Windows"), "{issues:?}");
}

/// Every kind of operation, on every platform: a folder on the way swapped for a link (a symlink, or
/// a junction on Windows) after the step's re-check makes the step fail, and nothing outside the
/// root is touched — neither pulled in (source side) nor written to (target side).
#[test]
fn a_link_swapped_in_redirects_no_kind_of_operation() {
	type Case = fn(&Path, &Path) -> Op; // (root, sub) -> op through sub
	let cases: [(&str, Case); 7] = [
		("move out of", |r, sub| Op::Move { from: sub.join("f.txt"), to: r.join("grabbed.txt") }),
		("move into", |r, sub| Op::Move { from: r.join("g.txt"), to: sub.join("g.txt") }),
		("copy out of", |r, sub| Op::Copy { from: sub.join("f.txt"), to: r.join("grabbed.txt") }),
		("rename in", |_, sub| Op::Rename { path: sub.join("f.txt"), new_name: "h.txt".into() }),
		("trash in", |_, sub| Op::Trash { path: sub.join("f.txt") }),
		("mkdir in", |_, sub| Op::Mkdir { path: sub.join("new") }),
		("rmdir in", |_, sub| Op::Rmdir { path: sub.join("empty") }),
	];
	for (what, case) in cases {
		let s = Sandbox::new();
		let sub = s.root.join("sub");
		fs::create_dir(&sub).unwrap();
		fs::write(sub.join("f.txt"), "mine").unwrap();
		fs::create_dir(sub.join("empty")).unwrap();
		fs::write(s.root.join("g.txt"), "G").unwrap();
		let outside = s.base.join("outside");
		fs::create_dir(&outside).unwrap();
		fs::write(outside.join("f.txt"), "secret").unwrap();
		fs::create_dir(outside.join("empty")).unwrap();
		let plan = Plan::new(vec![case(&s.root, &sub)], "");
		let (sub2, out2) = (sub.clone(), outside.clone());
		let v = s.validator();
		let mut j = s.journal();
		let result = Executor::new(&v, &mut j, s.state.join("trash"))
			.before_apply(move |_| {
				fs::rename(&sub2, sub2.with_file_name("sub-real")).unwrap();
				link_dir(&out2, &sub2);
			})
			.execute(&plan, Source::Human);
		assert!(result.is_err(), "{what}: went through the link");
		let mut outside_now: Vec<String> =
			fs::read_dir(&outside).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
		outside_now.sort();
		assert_eq!(outside_now, ["empty", "f.txt"], "{what}: the outside folder changed");
		assert_eq!(fs::read_to_string(outside.join("f.txt")).unwrap(), "secret", "{what}");
		assert!(!s.root.join("grabbed.txt").exists(), "{what}: an outside file was pulled in");
		assert_eq!(fs::read_to_string(s.root.join("g.txt")).unwrap(), "G", "{what}");
	}
}

/// An allowed root swapped for a link after validation does not bind the listing (or any
/// operation) to the link's target.
#[test]
fn a_root_swapped_for_a_link_is_not_followed() {
	let s = Sandbox::new();
	let roots = vec![s.root.clone()];
	let outside = s.base.join("outside");
	fs::create_dir(&outside).unwrap();
	fs::write(outside.join("secret.txt"), "secret").unwrap();
	fs::rename(&s.root, s.base.join("root-real")).unwrap();
	link_dir(&outside, &s.root);
	let listed = tpfs_core::list_confined(&roots, &s.root, 100);
	assert!(listed.is_err(), "listed the link's target: {listed:?}");
	assert!(tpfs_core::list_confined(&roots, &outside, 100).is_err(), "a folder outside the roots was listed");
}

/// The root itself swapped for a link while a plan runs: the steps still act on the real root
/// (through its handle) or fail, and nothing appears or disappears outside.
#[test]
fn a_root_swapped_mid_plan_never_redirects_a_step() {
	let s = Sandbox::new();
	fs::write(s.root.join("f.txt"), "mine").unwrap();
	let outside = s.base.join("outside");
	fs::create_dir(&outside).unwrap();
	fs::write(outside.join("f.txt"), "secret").unwrap();
	let plan = Plan::new(
		vec![
			Op::Mkdir { path: s.root.join("new") },
			Op::Trash { path: s.root.join("f.txt") },
			Op::Copy { from: s.root.join("new"), to: s.root.join("new2") },
		],
		"",
	);
	let (root2, out2) = (s.root.clone(), outside.clone());
	let v = s.validator();
	let mut j = s.journal();
	let mut swapped = false;
	let _ = Executor::new(&v, &mut j, s.state.join("trash"))
		.before_apply(move |_| {
			// Windows holds the root open without share-delete: the swap itself is refused there.
			if !swapped && fs::rename(&root2, root2.with_file_name("root-real")).is_ok() {
				link_dir(&out2, &root2);
				swapped = true;
			}
		})
		.execute(&plan, Source::Human);
	let mut now: Vec<String> =
		fs::read_dir(&outside).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
	now.sort();
	assert_eq!(now, ["f.txt"], "the outside folder changed");
	assert_eq!(fs::read_to_string(outside.join("f.txt")).unwrap(), "secret");
}

/// A crash right after a refused publish was recorded as failed, before its part was removed:
/// recovery removes the part and leaves the other program's file at the target alone.
#[test]
fn a_crash_after_a_refused_publish_never_takes_the_other_file() {
	let s = Sandbox::new();
	fs::write(s.root.join("a.txt"), "A").unwrap();
	let b = s.root.join("b.txt");
	let plan = Plan::new(vec![Op::Copy { from: s.root.join("a.txt"), to: b.clone() }], "");
	let rb = b.clone();
	let default_hook = panic::take_hook();
	panic::set_hook(Box::new(|_| {}));
	let crashed = panic::catch_unwind(AssertUnwindSafe(|| {
		let v = s.validator();
		let mut j = s.journal();
		let _ = Executor::new(&v, &mut j, s.state.join("trash"))
			.hook(move |_, p| match p {
				Phase::BeforeApply => fs::write(&rb, "THEIRS").unwrap(),
				Phase::FailedRecorded => panic!("simulated crash"),
				_ => {}
			})
			.execute(&plan, Source::Human);
	}));
	panic::set_hook(default_hook);
	assert!(crashed.is_err(), "the refused publish was not recorded before its clean-up");

	let v = s.validator();
	let mut j = s.journal();
	let report = Executor::new(&v, &mut j, s.state.join("trash")).recover().unwrap();
	assert!(report.errors.is_empty(), "{:?}", report.errors);
	assert_eq!(fs::read_to_string(&b).unwrap(), "THEIRS");
	let leftovers: Vec<_> = fs::read_dir(&s.root)
		.unwrap()
		.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
		.filter(|n| n.starts_with(".tpfs-part-"))
		.collect();
	assert!(leftovers.is_empty(), "{leftovers:?}");
	let trash = s.state.join("trash");
	assert!(!trash.exists() || fs::read_dir(&trash).unwrap().next().is_none(), "the other file went to the trash");
}

/// A private folder removed by `rmdir` comes back private: after a crash right after the
/// removal (before `done`), recovery restores its mode from the record written before it went.
#[cfg(unix)]
#[test]
fn a_crash_after_rmdir_restores_the_folders_own_mode() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	let d = s.root.join("private");
	fs::create_dir(&d).unwrap();
	fs::set_permissions(&d, fs::Permissions::from_mode(0o700)).unwrap();
	let plan = Plan::new(vec![Op::Rmdir { path: d.clone() }], "");
	let default_hook = panic::take_hook();
	panic::set_hook(Box::new(|_| {}));
	let crashed = panic::catch_unwind(AssertUnwindSafe(|| {
		let v = s.validator();
		let mut j = s.journal();
		let _ = Executor::new(&v, &mut j, s.state.join("trash"))
			.hook(|_, p| {
				if p == Phase::AfterApply {
					panic!("simulated crash")
				}
			})
			.execute(&plan, Source::Human);
	}));
	panic::set_hook(default_hook);
	assert!(crashed.is_err());
	assert!(!d.exists());
	let v = s.validator();
	let mut j = s.journal();
	let report = Executor::new(&v, &mut j, s.state.join("trash")).recover().unwrap();
	assert!(report.errors.is_empty(), "{:?}", report.errors);
	assert_eq!(fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
}
