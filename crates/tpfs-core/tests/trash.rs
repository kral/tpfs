//! `tpfs trash` and `tpfs trash empty`: listing, emptying, and what that means for undo.

use std::{fs, path::PathBuf};

use tempfile::TempDir;
use tpfs_core::{BatchStatus, Executor, Journal, Op, Plan, Source, Validator, trash};

struct Sandbox {
	_tmp: TempDir,
	#[cfg_attr(not(unix), allow(dead_code, reason = "used by the Unix-only symlink test"))]
	base: PathBuf,
	root: PathBuf,
	state: PathBuf,
}

impl Sandbox {
	fn new() -> Self {
		let tmp = TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		let (root, state) = (base.join("root"), base.join("state"));
		fs::create_dir(&root).unwrap();
		Self { _tmp: tmp, base, root, state }
	}

	fn journal(&self) -> Journal {
		Journal::open(self.state.join("journal.jsonl")).unwrap()
	}

	fn trash(&self) -> PathBuf {
		self.state.join("trash")
	}

	/// Trash the files `names` (made with their name as content) in one batch.
	fn trash_files(&self, j: &mut Journal, names: &[&str]) -> String {
		let ops = names
			.iter()
			.map(|n| {
				fs::write(self.root.join(n), n).unwrap();
				Op::Trash { path: self.root.join(n) }
			})
			.collect();
		let v = Validator::new([&self.root]).unwrap();
		Executor::new(&v, j, self.trash()).execute(&Plan::new(ops, "clean up"), Source::Human).unwrap().id
	}

	fn empty(&self, j: &mut Journal, ids: &[&str]) -> trash::Emptied {
		let all = trash::contents(j, &self.trash()).unwrap();
		let chosen: Vec<_> = all.into_iter().filter(|t| t.purged || ids.contains(&t.id.as_str())).collect();
		let ids: Vec<String> = chosen.iter().map(|t| t.id.clone()).collect();
		trash::empty(j, &self.trash(), &ids, &trash::token(&chosen)).unwrap()
	}
}

#[test]
fn the_trash_is_listed_per_batch() {
	let s = Sandbox::new();
	let mut j = s.journal();
	assert!(trash::contents(&mut j, &s.trash()).unwrap().is_empty());
	let a = s.trash_files(&mut j, &["one.txt", "three.txt"]);
	let b = s.trash_files(&mut j, &["two.txt"]);
	// A batch that trashes nothing has nothing in the trash.
	fs::write(s.root.join("m"), "m").unwrap();
	let v = Validator::new([&s.root]).unwrap();
	let plan = Plan::new(vec![Op::Move { from: s.root.join("m"), to: s.root.join("n") }], "move");
	Executor::new(&v, &mut j, s.trash()).execute(&plan, Source::Agent).unwrap();

	let all = trash::contents(&mut j, &s.trash()).unwrap();
	let shown: Vec<_> = all.iter().map(|t| (t.id.as_str(), t.items, t.bytes, t.undoable())).collect();
	assert_eq!(shown, [(a.as_str(), 2, 16, true), (b.as_str(), 1, 7, true)]);
	assert_eq!(all[0].source, "human");
	assert!(all[0].at > 0);
	assert_eq!(all[0].entries[0].1.as_deref(), Some(s.root.join("one.txt").as_path()), "where it came from");
}

#[test]
fn emptying_one_batch_leaves_the_others_and_undo_refuses_it() {
	let s = Sandbox::new();
	let mut j = s.journal();
	let a = s.trash_files(&mut j, &["a.txt"]);
	let b = s.trash_files(&mut j, &["b.txt"]);
	let done = s.empty(&mut j, &[&b]);
	assert_eq!((done.batches, done.items, done.bytes, done.errors.len()), (1, 1, 5, 0));
	assert!(!s.trash().join(&b).exists() && s.trash().join(&a).exists());

	let batches = j.batches().unwrap();
	let purged = batches.iter().find(|x| x.id == b).unwrap();
	assert!(purged.purged && purged.status == BatchStatus::Committed);
	let left = trash::contents(&mut j, &s.trash()).unwrap();
	assert_eq!(left.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), [a.as_str()]);

	// Undo stops at the emptied batch with a clear reason, and changes nothing.
	let v = Validator::new([&s.root]).unwrap();
	let err = Executor::new(&v, &mut j, s.trash()).undo_last().unwrap_err().to_string();
	assert!(err.contains("trash was emptied") && err.contains("can no longer be undone"), "{err}");
	assert!(!s.root.join("b.txt").exists() && s.trash().join(&a).exists());
}

#[test]
fn a_changed_selection_or_an_interrupted_batch_is_not_emptied() {
	let s = Sandbox::new();
	let mut j = s.journal();
	let a = s.trash_files(&mut j, &["a.txt"]);
	let all = trash::contents(&mut j, &s.trash()).unwrap();
	let tok = trash::token(&all);
	// Something more arrived in the batch's folder after it was shown.
	fs::write(s.trash().join(&a).join("0").join("extra"), "x").unwrap();
	let err = trash::empty(&mut j, &s.trash(), std::slice::from_ref(&a), &tok).unwrap_err();
	assert!(err.to_string().contains("changed since it was shown"), "{err}");
	assert!(s.trash().join(&a).join("0").join("a.txt").exists());
	assert!(!j.batches().unwrap()[0].purged, "nothing recorded");

	// A batch interrupted by a crash keeps its trash for `tpfs recover`.
	use tpfs_core::Entry;
	j.append(&Entry::Begin {
		batch: "18e0000000000000-1".into(),
		at: 1,
		source: "human".into(),
		reason: String::new(),
		digest: String::new(),
		v: 1,
	})
	.unwrap();
	fs::create_dir_all(s.trash().join("18e0000000000000-1").join("0")).unwrap();
	let all = trash::contents(&mut j, &s.trash()).unwrap();
	let open: Vec<_> = all.iter().filter(|t| t.id == "18e0000000000000-1").cloned().collect();
	assert!(!open[0].emptiable());
	let err = trash::empty(&mut j, &s.trash(), &["18e0000000000000-1".into()], &trash::token(&open)).unwrap_err();
	assert!(err.to_string().contains("tpfs recover"), "{err}");
}

/// A link in the trash is removed, not what it points to; read-only folders are emptied too.
#[cfg(unix)]
#[test]
fn links_in_the_trash_are_removed_without_touching_their_target() {
	use std::os::unix::fs::PermissionsExt;
	let s = Sandbox::new();
	let mut j = s.journal();
	let outside = s.base.join("outside");
	fs::create_dir(&outside).unwrap();
	fs::write(outside.join("keep.txt"), "keep").unwrap();
	// A link the user trashed, and one planted in the trash folder itself.
	std::os::unix::fs::symlink(&outside, s.root.join("link")).unwrap();
	let v = Validator::new([&s.root]).unwrap();
	let plan = Plan::new(vec![Op::Trash { path: s.root.join("link") }], "link");
	let id = Executor::new(&v, &mut j, s.trash()).execute(&plan, Source::Human).unwrap().id;
	let slot = s.trash().join(&id).join("0");
	std::os::unix::fs::symlink(&outside, slot.join("planted")).unwrap();
	std::os::unix::fs::symlink(outside.join("keep.txt"), slot.join("file-link")).unwrap();
	let ro = slot.join("ro");
	fs::create_dir(&ro).unwrap();
	fs::write(ro.join("x"), "x").unwrap();
	fs::set_permissions(&ro, fs::Permissions::from_mode(0o500)).unwrap();

	let done = s.empty(&mut j, &[&id]);
	assert!(done.errors.is_empty(), "{:?}", done.errors);
	assert!(!s.trash().join(&id).exists());
	assert_eq!(fs::read_to_string(outside.join("keep.txt")).unwrap(), "keep");
}

/// Journals written before `purge` existed read as before; a purge record without its time reads
/// too. An emptied batch that is archived stays known as emptied, so a cut-short run is finished.
#[test]
fn old_journals_still_read_and_compaction_keeps_purges() {
	let s = Sandbox::new();
	fs::create_dir_all(&s.state).unwrap();
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(&s.state, fs::Permissions::from_mode(0o700)).unwrap();
	}
	let line = |json: &str| format!("{:08x} {json}\n", crc32fast::hash(json.as_bytes()));
	let from = s.trash().join("18e0000000000000-a").join("0").join("x");
	let old = [
		r#"{"kind":"begin","batch":"18e0000000000000-a","at":5,"source":"human","reason":"old","digest":"d"}"#
			.to_owned(),
		format!(
			r#"{{"kind":"intent","batch":"18e0000000000000-a","index":0,"forward":{{"op":"trash","path":{:?}}},"inverse":{{"op":"move","from":{:?},"to":{:?}}}}}"#,
			s.root.join("x"),
			from,
			s.root.join("x")
		),
		r#"{"kind":"done","batch":"18e0000000000000-a","index":0,"mode":493}"#.to_owned(),
		r#"{"kind":"commit","batch":"18e0000000000000-a"}"#.to_owned(),
	];
	let text: String = old.iter().map(|l| line(l)).collect();
	fs::write(s.state.join("journal.jsonl"), &text).unwrap();
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(s.state.join("journal.jsonl"), fs::Permissions::from_mode(0o600)).unwrap();
	}
	let mut j = s.journal();
	let b = &j.batches().unwrap()[0];
	assert!(j.corrupt_lines() == 0 && b.status == BatchStatus::Committed && !b.purged && !b.damaged);

	// A purge record without `at`, and one for a batch only the archive knows: no damaged ghost.
	fs::write(
		s.state.join("journal.jsonl"),
		text + &line(r#"{"kind":"purge","batch":"18e0000000000000-a"}"#)
			+ &line(r#"{"kind":"purge","batch":"18e0000000000000-b"}"#),
	)
	.unwrap();
	drop(j);
	let mut j = s.journal();
	let batches = j.batches().unwrap();
	assert_eq!(batches.len(), 1);
	assert!(batches[0].purged && !batches[0].damaged && j.corrupt_lines() == 0);

	// Undone and emptied, then archived: still known as emptied, and its leftovers are finished.
	let id = s.trash_files(&mut j, &["u.txt"]);
	let v = Validator::new([&s.root]).unwrap();
	// The newest batch is undone; the old one (emptied) is below it.
	Executor::new(&v, &mut j, s.trash()).undo_last().unwrap();
	fs::create_dir_all(s.trash().join(format!("{id}-undo")).join("0")).unwrap();
	fs::write(s.trash().join(format!("{id}-undo")).join("0").join("left"), "l").unwrap();
	j.append(&tpfs_core::Entry::Purge { batch: id.clone(), at: 1 }).unwrap();
	assert!(j.compact().unwrap() >= 1);
	assert!(j.batches().unwrap().iter().all(|b| b.id != id), "archived");
	let all = trash::contents(&mut j, &s.trash()).unwrap();
	let t = all.iter().find(|t| t.id == id).unwrap();
	assert!(t.archived && t.purged && !t.undoable());
	let done = s.empty(&mut j, &[]);
	assert!(done.errors.is_empty());
	assert!(!s.trash().join(&id).exists() && !s.trash().join(format!("{id}-undo")).exists());
}
