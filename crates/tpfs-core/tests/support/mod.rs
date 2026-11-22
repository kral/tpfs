//! Shared by the crash-point and the randomized-plan tests: a sandbox with a root, a state folder
//! and a folder outside the root that must never change; an exact snapshot of a tree; and the
//! engine calls, each run under the crash recorder.
#![allow(dead_code)]

use std::{
	cell::Cell,
	collections::BTreeMap,
	fs,
	path::{Path, PathBuf},
	time::{Duration, SystemTime},
};

use tempfile::TempDir;
use tpfs_core::{
	BatchReport, BatchStatus, ExecError, Executor, Journal, Plan, RecoveryReport, Source, Validator,
	crash::{self, Crashed},
};

/// Every entry of a tree by its relative path: kind, permissions, and for files the modification
/// time and every byte (as hex), for symlinks the target.
pub type Snapshot = BTreeMap<String, String>;

/// What a run under the crash recorder gave: its result, or where it was stopped; and the crash
/// points it passed.
pub type Run<T> = (Result<T, Crashed>, Vec<&'static str>);

pub struct Sandbox {
	tmp: TempDir,
	/// A second root on another file system (Linux, where `/dev/shm` is one).
	other: Option<TempDir>,
	pub base: PathBuf,
	pub root: PathBuf,
	pub state: PathBuf,
	pub outside: PathBuf,
	pub root2: Option<PathBuf>,
	outside_before: Snapshot,
	files: Cell<u64>,
}

impl Sandbox {
	pub fn new() -> Self {
		Self::build(None)
	}

	/// A sandbox with a second root on another file system, or `None` where there is none.
	pub fn two_devices() -> Option<Self> {
		#[cfg(target_os = "linux")]
		{
			use std::os::unix::fs::MetadataExt;
			let other = TempDir::new_in("/dev/shm").ok()?;
			let here = TempDir::new().ok()?;
			let dev = |p: &Path| fs::metadata(p).map(|m| m.dev()).ok();
			(dev(other.path()) != dev(here.path())).then(|| Self::build(Some(other)))
		}
		#[cfg(not(target_os = "linux"))]
		None
	}

	fn build(other: Option<TempDir>) -> Self {
		let tmp = TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		let (root, state, outside) = (base.join("root"), base.join("state"), base.join("outside"));
		fs::create_dir(&root).unwrap();
		fs::create_dir_all(outside.join("dir")).unwrap();
		fs::write(outside.join("secret.txt"), "secret").unwrap();
		fs::write(outside.join("dir").join("inner.txt"), "inner").unwrap();
		let root2 = other.as_ref().map(|o| {
			let r = tpfs_core::canonicalize(o.path()).unwrap().join("root2");
			fs::create_dir(&r).unwrap();
			r
		});
		let outside_before = tree(&outside);
		Self { tmp, other, base, root, state, outside, root2, outside_before, files: Cell::new(0) }
	}

	pub fn roots(&self) -> Vec<PathBuf> {
		[Some(self.root.clone()), self.root2.clone()].into_iter().flatten().collect()
	}

	pub fn validator(&self) -> Validator {
		Validator::new(self.roots()).unwrap()
	}

	pub fn journal(&self) -> Journal {
		Journal::open(self.state.join("journal.jsonl")).unwrap()
	}

	pub fn trash(&self) -> PathBuf {
		self.state.join("trash")
	}

	/// Write a file with a fixed modification time of its own (a day apart from the others), so
	/// a snapshot also tells a file that was rewritten from one that was only moved.
	pub fn file(&self, path: impl AsRef<Path>, content: impl AsRef<[u8]>) {
		let n = self.files.get() + 1;
		self.files.set(n);
		fs::write(&path, content).unwrap();
		let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000 + n * 86_400);
		fs::File::options().write(true).open(&path).unwrap().set_modified(at).unwrap();
	}

	pub fn dir(&self, path: impl AsRef<Path>) {
		fs::create_dir_all(path).unwrap();
	}

	/// Everything below the roots, exactly.
	pub fn snapshot(&self) -> Snapshot {
		let mut all = tree(&self.root);
		if let Some(r) = &self.root2 {
			all.extend(tree(r).into_iter().map(|(k, v)| (format!("<root2>/{k}"), v)));
		}
		all
	}

	/// Nothing outside the roots changed: the folder next to the root is as it was, and nothing
	/// (a trash, a partial copy) appeared next to the root or the state folder.
	pub fn assert_contained(&self, what: &str) {
		assert_eq!(tree(&self.outside), self.outside_before, "{what}: the folder outside the root changed");
		let names = |dir: &Path| {
			let mut n: Vec<String> =
				fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
			n.sort();
			n
		};
		let mut expected = vec!["outside", "root"];
		if self.state.exists() {
			expected.push("state");
		}
		assert_eq!(names(&self.base), expected, "{what}: something appeared next to the root");
		if let Some(o) = &self.other {
			assert_eq!(names(o.path()), ["root2"], "{what}: something appeared next to the second root");
		}
	}

	pub fn statuses(&self) -> Vec<BatchStatus> {
		self.journal().batches().unwrap().iter().map(|b| b.status).collect()
	}

	/// Run the plan; `crash_at` stops it at that crash point, `before_step` runs before each step
	/// touches the disk (another program changing files meanwhile).
	pub fn execute(
		&self,
		plan: &Plan,
		crash_at: Option<usize>,
		before_step: &dyn Fn(usize),
	) -> Run<Result<BatchReport, ExecError>> {
		crash::run(crash_at, || {
			let v = self.validator();
			let mut j = self.journal();
			Executor::new(&v, &mut j, self.trash()).before_apply(before_step).execute(plan, Source::Human)
		})
	}

	pub fn undo(&self, crash_at: Option<usize>) -> Run<Result<Option<BatchReport>, ExecError>> {
		crash::run(crash_at, || {
			let v = self.validator();
			let mut j = self.journal();
			Executor::new(&v, &mut j, self.trash()).undo_last()
		})
	}

	pub fn recover(&self, crash_at: Option<usize>) -> Run<RecoveryReport> {
		crash::run(crash_at, || {
			let v = self.validator();
			let mut j = self.journal();
			Executor::new(&v, &mut j, self.trash()).recover().unwrap()
		})
	}
}

impl Drop for Sandbox {
	fn drop(&mut self) {
		// What this sandbox's batches left in the shared trash of the second file system.
		#[cfg(target_os = "linux")]
		if let Some(shared) = self.other.as_ref().and_then(|o| o.path().parent().map(Path::to_path_buf)) {
			let ids: Vec<String> = Journal::open(self.state.join("journal.jsonl"))
				.and_then(|mut j| j.batches())
				.map(|b| b.into_iter().map(|b| b.id).collect())
				.unwrap_or_default();
			let trashes = fs::read_dir(shared).into_iter().flatten().flatten();
			for trash in trashes.filter(|e| e.file_name().to_string_lossy().starts_with(".tpfs-trash-")) {
				for slot in fs::read_dir(trash.path()).into_iter().flatten().flatten() {
					let name = slot.file_name().to_string_lossy().into_owned();
					if ids.iter().any(|id| name.starts_with(id.as_str())) {
						make_removable(&slot.path());
						let _ = fs::remove_dir_all(slot.path());
					}
				}
			}
		}
		// Read-only folders would keep the temporary folders from being removed.
		make_removable(self.tmp.path());
		if let Some(o) = &self.other {
			make_removable(o.path());
		}
	}
}

fn make_removable(dir: &Path) {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let Ok(m) = fs::symlink_metadata(dir) else { return };
		if m.is_dir() {
			let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
			for e in fs::read_dir(dir).into_iter().flatten().flatten() {
				make_removable(&e.path());
			}
		}
	}
	#[cfg(not(unix))]
	let _ = dir;
}

/// Permission bits (Unix) or the read-only attribute (elsewhere).
pub fn mode_of(m: &fs::Metadata) -> String {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		format!("{:04o}", m.permissions().mode() & 0o7777)
	}
	#[cfg(not(unix))]
	{
		if m.permissions().readonly() { "ro".into() } else { "rw".into() }
	}
}

pub fn set_mode(path: impl AsRef<Path>, mode: u32) {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
	}
	#[cfg(not(unix))]
	let _ = (path, mode);
}

pub fn tree(dir: &Path) -> Snapshot {
	fn walk(base: &Path, dir: &Path, out: &mut Snapshot) {
		for e in fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display())) {
			let path = e.unwrap().path();
			let rel = path.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
			let m = fs::symlink_metadata(&path).unwrap();
			let mode = mode_of(&m);
			if m.file_type().is_symlink() {
				out.insert(rel, format!("link -> {}", fs::read_link(&path).unwrap().display()));
			} else if m.is_dir() {
				out.insert(rel, format!("dir {mode}"));
				walk(base, &path, out);
			} else {
				let bytes: String = fs::read(&path).unwrap().iter().map(|b| format!("{b:02x}")).collect();
				let mtime = m.modified().unwrap().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
				out.insert(rel, format!("file {mode} mtime={mtime} bytes={bytes}"));
			}
		}
	}
	let mut out = Snapshot::new();
	walk(dir, dir, &mut out);
	out
}

/// The differences between two snapshots, for a readable failure.
pub fn diff(now: &Snapshot, expected: &Snapshot) -> String {
	let mut out = String::new();
	for (k, v) in expected {
		match now.get(k) {
			None => out.push_str(&format!("  missing: {k} ({v})\n")),
			Some(n) if n != v => out.push_str(&format!("  changed: {k}\n    was {v}\n    now {n}\n")),
			_ => {}
		}
	}
	for (k, v) in now.iter().filter(|(k, _)| !expected.contains_key(*k)) {
		out.push_str(&format!("  extra:   {k} ({v})\n"));
	}
	out
}

#[track_caller]
pub fn assert_tree(s: &Sandbox, expected: &Snapshot, what: &str) {
	let now = s.snapshot();
	assert!(now == *expected, "{what}: the tree is not as it was:\n{}", diff(&now, expected));
	s.assert_contained(what);
}
