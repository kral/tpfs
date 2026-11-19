use std::{
	fs::{self, File},
	path::{Path, PathBuf},
	time::{Duration, SystemTime},
};

use tpfs_core::{
	Executor, Journal, Source, Validator,
	sync::{Diff, Direction, SyncOptions, compare, sync_plan},
};

struct Env {
	_tmp: tempfile::TempDir,
	base: PathBuf,
	left: PathBuf,
	right: PathBuf,
}

impl Env {
	fn new() -> Self {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		let (left, right) = (base.join("left"), base.join("right"));
		fs::create_dir_all(&left).unwrap();
		fs::create_dir_all(&right).unwrap();
		Self { _tmp: tmp, base, left, right }
	}

	/// A file with content and a modification time `age` seconds ago.
	fn file(&self, path: &Path, content: &str, age: u64) {
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, content).unwrap();
		let t = SystemTime::now() - Duration::from_secs(age);
		File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
	}

	fn run(&self, opts: SyncOptions) -> (usize, usize) {
		let cmp = compare(&self.left, &self.right, true).unwrap();
		let sp = sync_plan(&self.left, &self.right, &cmp, opts);
		let v = Validator::new([&self.base]).unwrap();
		assert!(v.validate(&sp.plan).is_empty(), "{:?}", v.validate(&sp.plan));
		let mut j = Journal::open(self.base.join("state/journal.jsonl")).unwrap();
		Executor::new(&v, &mut j, self.base.join("state/trash")).execute(&sp.plan, Source::Human).unwrap();
		(sp.plan.ops.len(), sp.skipped.len())
	}

	fn undo(&self) {
		let v = Validator::new([&self.base]).unwrap();
		let mut j = Journal::open(self.base.join("state/journal.jsonl")).unwrap();
		Executor::new(&v, &mut j, self.base.join("state/trash")).undo_last().unwrap().unwrap();
	}
}

fn read(p: PathBuf) -> String {
	fs::read_to_string(p).unwrap()
}

#[test]
fn compare_classifies_differences() {
	let e = Env::new();
	e.file(&e.left.join("same.txt"), "x", 100);
	e.file(&e.right.join("same.txt"), "x", 100);
	e.file(&e.left.join("only-left.txt"), "a", 100);
	e.file(&e.right.join("only-right.txt"), "b", 100);
	e.file(&e.left.join("newer-left.txt"), "new", 10);
	e.file(&e.right.join("newer-left.txt"), "old", 1000);
	e.file(&e.left.join("conflict.txt"), "aaaa", 100);
	e.file(&e.right.join("conflict.txt"), "b", 100);
	e.file(&e.left.join("deep/a/b.jpg"), "photo", 100);
	fs::create_dir_all(e.right.join("deep/a")).unwrap();
	fs::create_dir_all(e.left.join("type")).unwrap();
	e.file(&e.right.join("type"), "file", 100);

	let cmp = compare(&e.left, &e.right, true).unwrap();
	let got: Vec<(String, Diff)> = cmp.items.iter().map(|i| (i.rel.to_string_lossy().into_owned(), i.diff)).collect();
	assert_eq!(
		got,
		[
			("conflict.txt".into(), Diff::Conflict),
			(["deep", "a", "b.jpg"].iter().collect::<PathBuf>().to_string_lossy().into_owned(), Diff::OnlyLeft),
			("newer-left.txt".into(), Diff::LeftNewer),
			("only-left.txt".into(), Diff::OnlyLeft),
			("only-right.txt".into(), Diff::OnlyRight),
			("type".into(), Diff::TypeMismatch),
		]
	);
	assert_eq!(cmp.same, 1);

	// Not recursive: folders on both sides are not opened.
	let flat = compare(&e.left, &e.right, false).unwrap();
	assert!(flat.items.iter().all(|i| i.rel.components().count() == 1));
}

#[test]
fn one_way_sync_copies_and_replaces_without_overwriting() {
	let e = Env::new();
	e.file(&e.left.join("new.jpg"), "new", 100);
	e.file(&e.left.join("album/a.jpg"), "a", 100);
	e.file(&e.left.join("edited.txt"), "v2", 10);
	e.file(&e.right.join("edited.txt"), "v1", 1000);
	e.file(&e.right.join("newer-on-right.txt"), "keep", 10);
	e.file(&e.left.join("newer-on-right.txt"), "older", 1000);
	e.file(&e.right.join("extra.txt"), "extra", 100);

	let (ops, skipped) = e.run(SyncOptions { direction: Direction::LeftToRight, mirror: false });
	assert_eq!(ops, 4, "copy new.jpg, copy album, trash+copy edited.txt");
	assert_eq!(skipped, 2, "newer on the right, extra without mirror");
	assert_eq!(read(e.right.join("new.jpg")), "new");
	assert_eq!(read(e.right.join("album/a.jpg")), "a");
	assert_eq!(read(e.right.join("edited.txt")), "v2");
	assert_eq!(read(e.right.join("newer-on-right.txt")), "keep");
	assert!(e.right.join("extra.txt").exists());

	// Everything comes back, including the replaced old version.
	e.undo();
	assert_eq!(read(e.right.join("edited.txt")), "v1");
	assert!(!e.right.join("new.jpg").exists());
	assert!(!e.right.join("album").exists());
}

#[test]
fn mirror_trashes_extras_and_two_way_lets_newer_win() {
	let e = Env::new();
	e.file(&e.left.join("a.txt"), "a", 100);
	e.file(&e.right.join("extra.txt"), "extra", 100);
	e.run(SyncOptions { direction: Direction::LeftToRight, mirror: true });
	assert!(e.right.join("a.txt").exists());
	assert!(!e.right.join("extra.txt").exists(), "mirror moved it to the trash");
	e.undo();
	assert!(e.right.join("extra.txt").exists());

	let e = Env::new();
	e.file(&e.left.join("l.txt"), "l", 100);
	e.file(&e.right.join("r.txt"), "r", 100);
	e.file(&e.left.join("both.txt"), "old", 1000);
	e.file(&e.right.join("both.txt"), "new", 10);
	// Mirror is ignored for two-way sync: nothing is trashed except replaced old versions.
	e.run(SyncOptions { direction: Direction::Both, mirror: true });
	assert_eq!(read(e.left.join("r.txt")), "r");
	assert_eq!(read(e.right.join("l.txt")), "l");
	assert_eq!(read(e.left.join("both.txt")), "new");
	assert_eq!(compare(&e.left, &e.right, true).unwrap().items.len(), 0);
}

#[test]
fn identical_folders_give_an_empty_plan() {
	let e = Env::new();
	e.file(&e.left.join("x/y.txt"), "same", 100);
	e.file(&e.right.join("x/y.txt"), "same", 100);
	let cmp = compare(&e.left, &e.right, true).unwrap();
	assert!(cmp.items.is_empty());
	assert_eq!(cmp.same, 1);
	let sp = sync_plan(&e.left, &e.right, &cmp, SyncOptions::default());
	assert!(sp.plan.ops.is_empty());
}

#[test]
fn replacing_files_is_medium_risk() {
	use tpfs_core::risk::{Level, assess};
	let e = Env::new();
	e.file(&e.left.join("a.txt"), "new", 10);
	e.file(&e.right.join("a.txt"), "old", 1000);
	e.file(&e.left.join("b.txt"), "b", 10);
	let cmp = compare(&e.left, &e.right, true).unwrap();
	let sp = sync_plan(&e.left, &e.right, &cmp, SyncOptions::default());
	let risk = assess(&sp.plan, Source::Human);
	assert_eq!(risk.level, Level::Medium);
	assert!(risk.reasons.iter().any(|r| r.contains("replaces 1 existing file")), "{:?}", risk.reasons);
}
