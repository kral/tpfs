//! A crash at every point of a plan, of its undo and of its recovery.
//!
//! Each scenario is first run under the crash recorder (`tpfs_core::crash`), which lists every
//! point where tpfs is about to change the disk or its journal — nothing is listed by hand, so a
//! new step in the engine is covered as soon as it exists. The scenario is then run again once
//! per point, stopped there (for journal appends also with half the line written), and:
//!
//! * `recover` must put the tree back exactly — every byte, mode and file time — and leave the
//!   journal usable: the same plan then runs and is undone;
//! * recovery is itself stopped at each of its points, and a second `recover` must finish it;
//! * an undo stopped at each of its points (and its rerun stopped again at each of its own) must
//!   be finished by undoing again.
//!
//! The second crash (in the recovery, in the rerun undo) multiplies the runs. A normal `cargo
//! test` therefore takes every [`NESTED_EVERY`]-th pair of first and second crash point, spread
//! evenly, and leaves out the torn variant of a journal append there (the fragment is skipped on
//! reading, so the repair after it is the one after the append that never started). With
//! `TPFS_CRASH_NESTED_EVERY=1` every pair is run, as CI does on `main` and weekly. First crashes
//! after which the repair once went wrong are listed with their scenario (`always`, `undo_always`)
//! and get every second crash in any run.

mod support;

use std::{cell::Cell, fs, path::Path};

use support::{Sandbox, Snapshot, assert_tree, set_mode};
use tpfs_core::{BatchStatus, ExecError, Op, Plan};

/// Of the pairs (crash in the plan or undo, crash in the repair after it), one in this many is run.
const NESTED_EVERY: usize = 6;

fn nested_every() -> usize {
	match std::env::var("TPFS_CRASH_NESTED_EVERY") {
		Ok(v) => v.parse().ok().filter(|n| *n > 0).expect("TPFS_CRASH_NESTED_EVERY must be a positive number"),
		Err(_) => NESTED_EVERY,
	}
}

/// Whether the pair of the `i`-th first crash (at `first`) and the `k`-th second one (at `second`)
/// is run.
fn nested(every: usize, (i, first): (usize, &str), (k, second): (usize, &str)) -> bool {
	every == 1 || ((i + k) % every == 0 && first != "journal.torn" && second != "journal.torn")
}

struct Scenario {
	name: &'static str,
	/// Builds the tree and returns the plan.
	build: fn(&Sandbox) -> Plan,
	/// Another program removes this file (relative to the root) right before the given step: the
	/// step fails and the batch is rolled back inside `execute`.
	vanishes: Option<(usize, &'static str)>,
	two_devices: bool,
	/// Crash points of the plan (name, which occurrence of it) after which every crash point of
	/// the recovery is always tried, whatever `TPFS_CRASH_NESTED_EVERY` says.
	always: &'static [(&'static str, usize)],
	/// The same for crash points of the undo and the undo run again.
	undo_always: &'static [(&'static str, usize)],
}

/// Whether point `i` of `points` is one of `listed` (name, occurrence).
fn is_listed(points: &[&str], i: usize, listed: &[(&str, usize)]) -> bool {
	let occurrence = points[..i].iter().filter(|p| **p == points[i]).count();
	listed.contains(&(points[i], occurrence))
}

#[derive(Default)]
struct Coverage {
	/// Distinct crash points: of the plan, of its undo, and of the recoveries and reruns after them.
	points: usize,
	/// Runs stopped by a crash and then repaired.
	crashes: usize,
}

fn sandbox(sc: &Scenario) -> Option<(Sandbox, Plan)> {
	let s = if sc.two_devices { Sandbox::two_devices()? } else { Sandbox::new() };
	let plan = (sc.build)(&s);
	Some((s, plan))
}

/// Run the plan, stopped at `crash_at` if given. Returns, with the run's result and crash points,
/// the tree to expect once the batch is gone again: as before, less the file another program
/// removed if it got that far.
#[allow(clippy::type_complexity)]
fn run_plan(
	sc: &Scenario,
	s: &Sandbox,
	plan: &Plan,
	crash_at: Option<usize>,
) -> (Result<Result<tpfs_core::BatchReport, ExecError>, tpfs_core::crash::Crashed>, Vec<&'static str>, Snapshot) {
	let mut expected = s.snapshot();
	let vanished = Cell::new(false);
	let (result, points) = s.execute(plan, crash_at, &|step| {
		if let Some((at, rel)) = sc.vanishes
			&& at == step
		{
			fs::remove_file(s.root.join(rel)).unwrap();
			vanished.set(true);
		}
	});
	if let (true, Some((_, rel))) = (vanished.get(), sc.vanishes) {
		expected.remove(rel).unwrap();
	}
	(result, points, expected)
}

/// After a recovery: no errors, the tree as expected, no batch left open.
fn assert_recovered(s: &Sandbox, report: &tpfs_core::RecoveryReport, expected: &Snapshot, what: &str) {
	assert!(report.errors.is_empty(), "{what}: recovery reported {:?}", report.errors);
	assert_tree(s, expected, what);
	let statuses = s.statuses();
	assert!(statuses.iter().all(|st| *st == BatchStatus::Aborted), "{what}: batches are {statuses:?}");
}

fn check(sc: &Scenario) -> Coverage {
	let mut cov = Coverage::default();
	let every = nested_every();
	let name = sc.name;
	let Some((s, plan)) = sandbox(sc) else {
		eprintln!("{name}: skipped, no second file system here");
		return cov;
	};

	// The reference run: which points there are, and that the plan and its undo work at all.
	let before = s.snapshot();
	let (result, points, expected) = run_plan(sc, &s, &plan, None);
	let mut undo_points = Vec::new();
	match result.expect("no crash was asked for") {
		Ok(report) => {
			assert!(sc.vanishes.is_none(), "{name}: expected to fail");
			assert_eq!(report.steps, plan.ops.len(), "{name}");
			assert_ne!(s.snapshot(), before, "{name}: the plan changed nothing");
			let (undone, p) = s.undo(None);
			undo_points = p;
			assert!(matches!(undone, Ok(Ok(Some(_)))), "{name}: undo: {undone:?}");
			assert_tree(&s, &before, &format!("{name}: undo"));
			assert!(matches!(s.undo(None).0, Ok(Ok(None))), "{name}: undone twice");
		}
		Err(ExecError::Failed { rollback_errors, .. }) => {
			assert!(sc.vanishes.is_some(), "{name}: failed");
			assert!(rollback_errors.is_empty(), "{name}: {rollback_errors:?}");
			assert_tree(&s, &expected, &format!("{name}: rolled back"));
			assert_eq!(s.statuses(), [BatchStatus::Aborted], "{name}");
		}
		Err(e) => panic!("{name}: {e}"),
	}
	assert!(!points.is_empty(), "{name}: no crash points");
	for (listed, points) in [(sc.always, &points), (sc.undo_always, &undo_points)] {
		for l in listed {
			assert!((0..points.len()).any(|i| is_listed(points, i, &[*l])), "{name}: no crash point {l:?}");
		}
	}
	cov.points += points.len() + undo_points.len();

	// A crash at every point of the plan, then at every point of the recovery after it.
	for (i, at) in points.iter().enumerate() {
		let what = format!("{name}: crash at #{i} ({at})");
		let (s, plan) = sandbox(sc).unwrap();
		let before = s.snapshot();
		let (result, _, expected) = run_plan(sc, &s, &plan, Some(i));
		let crashed = result.expect_err(&what);
		assert_eq!((crashed.at, crashed.index), (*at, i), "{what}: the run is not deterministic");

		let (report, recover_points) = s.recover(None);
		assert_recovered(&s, &report.unwrap(), &expected, &what);
		cov.crashes += 1;
		// Nothing is left that would be in the way: the plan runs now, and is undone.
		if sc.vanishes.is_none() {
			let (again, _) = s.execute(&plan, None, &|_| {});
			assert!(matches!(again, Ok(Ok(_))), "{what}: the plan again: {again:?}");
			assert!(matches!(s.undo(None).0, Ok(Ok(Some(_)))), "{what}: undo of the plan run again");
			assert_tree(&s, &before, &format!("{what}, run again and undone"));
		}

		cov.points += recover_points.len();
		let every = if is_listed(&points, i, sc.always) { 1 } else { every };
		for (k, rat) in recover_points.iter().enumerate().filter(|(k, rat)| nested(every, (i, at), (*k, rat))) {
			let what = format!("{what}, then in recovery at #{k} ({rat})");
			let (s, plan) = sandbox(sc).unwrap();
			let (result, _, expected) = run_plan(sc, &s, &plan, Some(i));
			result.expect_err(&what);
			let crashed = s.recover(Some(k)).0.expect_err(&what);
			assert_eq!(crashed.at, *rat, "{what}: the recovery is not deterministic");
			// Recovery is idempotent: run again it finishes, and a third run finds nothing to do.
			assert_recovered(&s, &s.recover(None).0.unwrap(), &expected, &what);
			let third = s.recover(None).0.unwrap();
			assert!(third.rolled_back.is_empty() && third.errors.is_empty(), "{what}: third recovery: {third:?}");
			assert_tree(&s, &expected, &format!("{what}, recovered again"));
			cov.crashes += 1;
		}
	}

	// A crash at every point of the undo, then at every point of the undo run again.
	for (j, at) in undo_points.iter().enumerate() {
		let what = format!("{name}: undo crashes at #{j} ({at})");
		let interrupted_undo = || {
			let (s, plan) = sandbox(sc).unwrap();
			let before = s.snapshot();
			let (done, ..) = s.execute(&plan, None, &|_| {});
			assert!(matches!(done, Ok(Ok(_))), "{what}: {done:?}");
			let crashed = s.undo(Some(j)).0.expect_err(&what);
			assert_eq!(crashed.at, *at, "{what}: the undo is not deterministic");
			// A committed batch is not recovery's business, half undone or not.
			let report = s.recover(None).0.unwrap();
			assert!(report.rolled_back.is_empty() && report.errors.is_empty(), "{what}: recovery: {report:?}");
			(s, before)
		};
		let finish = |s: &Sandbox, before: &Snapshot, what: &str| {
			let (undone, points) = s.undo(None);
			assert!(matches!(undone, Ok(Ok(Some(_)))), "{what}: undo again: {undone:?}");
			assert_tree(s, before, what);
			assert_eq!(s.statuses(), [BatchStatus::Undone], "{what}");
			points
		};
		let (s, before) = interrupted_undo();
		let again_points = finish(&s, &before, &what);
		cov.crashes += 1;

		cov.points += again_points.len();
		let every = if is_listed(&undo_points, j, sc.undo_always) { 1 } else { every };
		for (k, aat) in again_points.iter().enumerate().filter(|(k, aat)| nested(every, (j, at), (*k, aat))) {
			let what = format!("{what}, then undoing again at #{k} ({aat})");
			let (s, before) = interrupted_undo();
			let crashed = s.undo(Some(k)).0.expect_err(&what);
			assert_eq!(crashed.at, *aat, "{what}: the second undo is not deterministic");
			finish(&s, &before, &what);
			cov.crashes += 1;
		}
	}
	eprintln!("{name}: {} crash points, {} crashes repaired", cov.points, cov.crashes);
	cov
}

/// A small tree with a bit of everything: files, an empty file, bytes that are not text, a nested
/// folder with modes of its own and (Unix) symlinks, one of them dangling, and a read-only folder
/// (which, unless the tests run as root, nothing can be taken out of or put into).
fn tree(s: &Sandbox, root: &Path) {
	s.file(root.join("a.txt"), "A");
	s.file(root.join("b.txt"), "B");
	s.file(root.join("empty"), "");
	s.dir(root.join("docs/deep/deeper"));
	s.file(root.join("docs/one.md"), "one");
	s.file(root.join("docs/deep/two.bin"), [0u8, 159, 146, 150, 255]);
	s.file(root.join("docs/deep/deeper/three"), "3".repeat(5000));
	s.dir(root.join("docs/hollow"));
	s.dir(root.join("spare"));
	s.dir(root.join("vault/inner"));
	s.file(root.join("vault/key"), "k");
	s.file(root.join("vault/inner/x"), "x");
	#[cfg(unix)]
	{
		std::os::unix::fs::symlink("one.md", root.join("docs/link")).unwrap();
		std::os::unix::fs::symlink("nowhere", root.join("docs/deep/dangling")).unwrap();
	}
	set_mode(root.join("a.txt"), 0o600);
	set_mode(root.join("docs/one.md"), 0o444);
	set_mode(root.join("docs/deep/deeper/three"), 0o755);
	set_mode(root.join("docs/deep/deeper"), 0o700);
	set_mode(root.join("docs/hollow"), 0o750);
	set_mode(root.join("spare"), 0o2775);
	set_mode(root.join("docs"), 0o711);
	set_mode(root.join("vault/key"), 0o400);
	set_mode(root.join("vault/inner"), 0o500);
	set_mode(root.join("vault"), 0o555);
}

macro_rules! scenarios {
	($($(#[$attr:meta])* $name:ident $([$($opt:ident = $val:expr),*])? => |$s:ident, $r:ident| $ops:expr;)*) => {
		$(
			$(#[$attr])*
			#[test]
			fn $name() {
				fn build($s: &Sandbox) -> Plan {
					let $r = &$s.root;
					tree($s, $r);
					Plan::new($ops, stringify!($name))
				}
				#[allow(unused_mut)]
				let mut sc = Scenario {
					name: stringify!($name),
					build,
					vanishes: None,
					two_devices: false,
					always: &[],
					undo_always: &[],
				};
				$($(sc.$opt = $val;)*)?
				let cov = check(&sc);
				assert!(sc.two_devices || cov.crashes > 0);
			}
		)*
	};
}

scenarios! {
	// Each kind of operation on its own.
	// An undo cut short in the middle of a journal append, and cut short again when run again,
	// was refused for good as "damaged".
	move_a_file [undo_always = &[("journal.torn", 0)]] => |s, r| vec![
		Op::Move { from: r.join("a.txt"), to: r.join("spare/a.txt") },
	];
	move_a_folder => |s, r| vec![Op::Move { from: r.join("docs"), to: r.join("spare/papers") }];
	copy_a_file => |s, r| vec![Op::Copy { from: r.join("docs/one.md"), to: r.join("one-copy.md") }];
	copy_a_folder_tree => |s, r| vec![Op::Copy { from: r.join("docs"), to: r.join("spare/docs-copy") }];
	rename_a_file => |s, r| vec![Op::Rename { path: r.join("a.txt"), new_name: "z.txt".into() }];
	rename_a_folder => |s, r| vec![Op::Rename { path: r.join("docs"), new_name: "papers".into() }];
	// Through a temporary name where the file system ignores case; a plain rename elsewhere.
	rename_only_the_letter_case => |s, r| vec![
		Op::Rename { path: r.join("a.txt"), new_name: "A.TXT".into() },
		Op::Rename { path: r.join("docs"), new_name: "Docs".into() },
	];
	trash_a_file => |s, r| vec![Op::Trash { path: r.join("b.txt") }];
	trash_a_folder => |s, r| vec![Op::Trash { path: r.join("docs") }];
	mkdir => |s, r| vec![Op::Mkdir { path: r.join("spare/new") }];
	rmdir => |s, r| vec![Op::Rmdir { path: r.join("docs/hollow") }];
	rmdir_of_a_folder_with_special_mode => |s, r| vec![Op::Rmdir { path: r.join("spare") }];

	// The copy has the folder's mode: undo and recovery must still be able to take it away.
	copy_a_read_only_folder => |s, r| vec![
		Op::Copy { from: r.join("vault"), to: r.join("spare/vault") },
		Op::Rename { path: r.join("vault"), new_name: "safe".into() },
	];

	// Plans whose steps build on each other.
	move_onto_a_new_subfolder => |s, r| vec![
		Op::Mkdir { path: r.join("new") },
		Op::Mkdir { path: r.join("new/sub") },
		Op::Move { from: r.join("a.txt"), to: r.join("new/sub/a.txt") },
		Op::Move { from: r.join("docs"), to: r.join("new/sub/docs") },
	];
	one_of_each => |s, r| vec![
		Op::Mkdir { path: r.join("new") },
		Op::Move { from: r.join("a.txt"), to: r.join("new/a.txt") },
		Op::Copy { from: r.join("docs"), to: r.join("new/docs-copy") },
		Op::Rename { path: r.join("b.txt"), new_name: "renamed.txt".into() },
		Op::Trash { path: r.join("empty") },
		Op::Rmdir { path: r.join("spare") },
	];
	swap_two_names => |s, r| vec![
		Op::Rename { path: r.join("a.txt"), new_name: "swap.tmp".into() },
		Op::Rename { path: r.join("b.txt"), new_name: "a.txt".into() },
		Op::Rename { path: r.join("swap.tmp"), new_name: "b.txt".into() },
	];
	empty_a_folder_and_remove_it => |s, r| vec![
		Op::Move { from: r.join("docs/deep/deeper/three"), to: r.join("three") },
		Op::Rmdir { path: r.join("docs/deep/deeper") },
		Op::Trash { path: r.join("docs/deep/two.bin") },
		Op::Rmdir { path: r.join("docs/hollow") },
	];
	remove_a_folder_and_make_it_again => |s, r| vec![
		Op::Rmdir { path: r.join("spare") },
		Op::Mkdir { path: r.join("spare") },
		Op::Move { from: r.join("a.txt"), to: r.join("spare/a.txt") },
		Op::Rmdir { path: r.join("docs/hollow") },
		Op::Mkdir { path: r.join("docs/hollow") },
	];
	trash_a_folder_and_put_another_in_its_place => |s, r| vec![
		Op::Trash { path: r.join("docs") },
		Op::Mkdir { path: r.join("docs") },
		Op::Copy { from: r.join("a.txt"), to: r.join("docs/fresh.md") },
		Op::Trash { path: r.join("a.txt") },
		Op::Move { from: r.join("b.txt"), to: r.join("a.txt") },
	];
	// Later steps change the copy; its undo must still know it as its own.
	copy_a_folder_and_build_on_the_copy => |s, r| vec![
		Op::Copy { from: r.join("docs"), to: r.join("copy") },
		Op::Move { from: r.join("a.txt"), to: r.join("copy/a.txt") },
		Op::Rename { path: r.join("copy"), new_name: "kept".into() },
		Op::Copy { from: r.join("b.txt"), to: r.join("kept/b.txt") },
		Op::Trash { path: r.join("kept/b.txt") },
		Op::Copy { from: r.join("kept"), to: r.join("spare/kept-too") },
	];
	trash_twice_under_the_same_name => |s, r| vec![
		Op::Trash { path: r.join("a.txt") },
		Op::Rename { path: r.join("b.txt"), new_name: "a.txt".into() },
		Op::Trash { path: r.join("a.txt") },
	];
	// A step fails half way through the plan: the roll-back inside `execute` is crashed too.
	a_step_fails_and_the_batch_is_rolled_back [vanishes = Some((4, "b.txt"))] => |s, r| vec![
		Op::Mkdir { path: r.join("new") },
		Op::Copy { from: r.join("docs"), to: r.join("new/docs") },
		Op::Trash { path: r.join("a.txt") },
		Op::Rmdir { path: r.join("spare") },
		Op::Move { from: r.join("b.txt"), to: r.join("new/b.txt") },
	];

	// Across file systems a move is a copy, a journal record, a rename and a removal.
	// After the move and before its `done` record, recovery copies the file back; cut short in
	// that, it left a partial copy that kept every later recovery from making the copy.
	move_a_file_to_another_file_system [two_devices = true, always = &[("journal.write", 3)]] => |s, r| vec![
		Op::Move { from: r.join("a.txt"), to: s.root2.as_ref().unwrap().join("a.txt") },
	];
	// Half the source removed: recovery puts it back from the copy; cut short in that, it left
	// its private partial copy in the user's folder.
	move_a_folder_to_another_file_system [two_devices = true, always = &[("fs.unlink", 3)]] => |s, r| vec![
		Op::Move { from: r.join("docs"), to: s.root2.as_ref().unwrap().join("docs") },
	];
	move_a_read_only_folder_to_another_file_system [two_devices = true] => |s, r| vec![
		Op::Move { from: r.join("vault"), to: s.root2.as_ref().unwrap().join("vault") },
	];
	copy_and_trash_on_another_file_system [two_devices = true] => |s, r| vec![
		Op::Mkdir { path: s.root2.as_ref().unwrap().join("in") },
		Op::Copy { from: r.join("docs"), to: s.root2.as_ref().unwrap().join("in/docs") },
		Op::Move { from: r.join("b.txt"), to: s.root2.as_ref().unwrap().join("in/b.txt") },
		Op::Trash { path: s.root2.as_ref().unwrap().join("in/b.txt") },
		Op::Move { from: s.root2.as_ref().unwrap().join("in/docs"), to: r.join("back") },
	];
}
