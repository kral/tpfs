//! Randomized plans over randomized trees, from a seed.
//!
//! Each case builds a small random tree (odd names, modes, symlinks, read-only folders) and a
//! random plan over it — mostly sensible, sometimes hostile (paths outside the root, into tpfs's
//! own state, through symlinks, relative, absurd names) — and checks what must hold for any plan:
//!
//! * validating, previewing and running it never panics;
//! * a plan that is refused, or accepted and then fails at a step, leaves the tree exactly as it
//!   was (every byte, mode and file time);
//! * a plan that commits is undone exactly;
//! * a crash at a random point of the run, of the recovery or of the undo is repaired exactly;
//! * nothing outside the root changes, and nothing (no trash, no partial copy) appears next to it.
//!
//! `cargo test` runs [`CASES`] cases from a fixed seed. `TPFS_FUZZ_CASES` sets another number and
//! `TPFS_FUZZ_SEED` the seed of the first case (case `k` uses seed + `k`). A failing case prints
//! the line that runs it again alone.

mod support;

use std::{
	fs,
	path::{Path, PathBuf},
};

use support::{Sandbox, assert_tree, set_mode};
use tpfs_core::{ExecError, Op, Plan};

/// Cases in a normal `cargo test`.
const CASES: u64 = 1000;
const SEED: u64 = 0x7F5_2026;

/// xorshift64*, seeded through splitmix64 (so neighbouring seeds give unrelated streams).
struct Rng(u64);

impl Rng {
	fn new(seed: u64) -> Self {
		let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
		z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
		z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
		Self((z ^ (z >> 31)) | 1)
	}

	fn next(&mut self) -> u64 {
		self.0 ^= self.0 >> 12;
		self.0 ^= self.0 << 25;
		self.0 ^= self.0 >> 27;
		self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
	}

	fn below(&mut self, n: usize) -> usize {
		((self.next() >> 33) as usize) % n.max(1)
	}

	fn chance(&mut self, percent: usize) -> bool {
		self.below(100) < percent
	}

	fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
		&items[self.below(items.len())]
	}
}

/// Names that collide where the file system ignores case or Unicode form, and a few odd ones.
const NAMES: &[&str] =
	&["a", "b", "c", "A", "B", "d.txt", "D.TXT", "e e", "\u{e9}", "e\u{301}", "x.y.z", ".hidden", "\u{df}", "n1", "n2"];
const NEW_NAMES: &[&str] = &["new", "new2", "Z", "a", "b", "c", "A", "d.txt", "\u{e9}", "moved here"];

/// What the generator believes exists, kept roughly in step with the plan so far (the plan may
/// still be wrong: that is wanted too).
struct World {
	dirs: Vec<PathBuf>,
	entries: Vec<PathBuf>,
	links: Vec<PathBuf>,
}

impl World {
	fn forget(&mut self, path: &Path) {
		self.dirs.retain(|d| !d.starts_with(path));
		self.entries.retain(|e| !e.starts_with(path));
	}

	fn add(&mut self, path: &Path, dir: bool) {
		self.entries.push(path.to_path_buf());
		if dir {
			self.dirs.push(path.to_path_buf());
		}
	}

	fn is_dir(&self, path: &Path) -> bool {
		self.dirs.iter().any(|d| d == path)
	}
}

fn random_tree(rng: &mut Rng, s: &Sandbox) -> World {
	let roots = s.roots();
	let mut w = World { dirs: roots.clone(), entries: Vec::new(), links: Vec::new() };
	let mut modes: Vec<(PathBuf, u32)> = Vec::new();
	for _ in 0..3 + rng.below(12) {
		let parent = rng.pick(&w.dirs).clone();
		let path = parent.join(rng.pick(NAMES));
		let depth = roots.iter().filter_map(|r| path.strip_prefix(r).ok()).map(|p| p.components().count()).max();
		if fs::symlink_metadata(&path).is_ok() || depth > Some(4) {
			continue;
		}
		match rng.below(100) {
			0..35 => {
				if fs::create_dir(&path).is_err() {
					continue;
				}
				let mode = if rng.chance(8) { 0o555 } else { *rng.pick(&[0o755, 0o700, 0o775, 0o750]) };
				modes.push((path.clone(), mode));
				w.dirs.push(path.clone());
			}
			#[cfg(unix)]
			35..45 => {
				let target = match rng.below(5) {
					0 => PathBuf::from("nowhere"),
					1 => s.outside.join("dir"),
					2 => s.outside.join("secret.txt"),
					3 => PathBuf::from(".."),
					_ => PathBuf::from(rng.pick(NAMES)),
				};
				std::os::unix::fs::symlink(target, &path).unwrap();
				w.links.push(path.clone());
			}
			_ => {
				let len = match rng.below(10) {
					0 => 0,
					1 => 5000,
					_ => 1 + rng.below(64),
				};
				let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
				if fs::write(&path, b"").is_err() {
					continue;
				}
				s.file(&path, bytes);
				modes.push((path.clone(), *rng.pick(&[0o644, 0o600, 0o444, 0o755, 0o640])));
			}
		}
		w.entries.push(path);
	}
	// Children first, so a read-only folder is filled before it is closed.
	for (path, mode) in modes.iter().rev() {
		set_mode(path, *mode);
	}
	w
}

/// A path no plan may use, or one that is merely absurd.
fn hostile_path(rng: &mut Rng, s: &Sandbox, w: &World) -> PathBuf {
	let root = &s.root;
	match rng.below(22) {
		0 => root.clone(),
		1 => s.base.clone(),
		2 => s.outside.join("secret.txt"),
		3 => s.outside.join("dir"),
		4 => s.state.join("journal.jsonl"),
		5 => s.state.clone(),
		6 => s.state.join("trash").join("x"),
		7 => PathBuf::from("relative/path"),
		8 => PathBuf::new(),
		9 => root.join("..").join("outside").join("secret.txt"),
		10 => root.join("a").join("..").join("..").join("outside").join("dir"),
		11 => root.join(".tpfs-part-0123abcd-0"),
		12 => root.join(".tpfs-trash-0").join("y"),
		13 => root.join(".TPFS-case-0123abcd-0"),
		14 => w.links.first().map_or_else(|| root.join("no-link"), |l| l.join("x")),
		15 => root.join("n".repeat(300)),
		16 => root.join("line\nbreak\u{1b}[31m"),
		17 => root.ancestors().last().unwrap().to_path_buf(),
		18 => root.join("con"),
		19 => root.join("a:b"),
		20 => root.join("trailing. "),
		_ => {
			#[cfg(unix)]
			{
				use std::os::unix::ffi::OsStrExt;
				root.join(std::ffi::OsStr::from_bytes(b"not-utf8-\xff"))
			}
			#[cfg(not(unix))]
			root.join("\u{202e}reversed")
		}
	}
}

fn random_plan(rng: &mut Rng, s: &Sandbox, w: &mut World) -> Plan {
	let hostile = rng.chance(25);
	let steps = if rng.chance(2) { 0 } else { 1 + rng.below(6) };
	let mut ops = Vec::new();
	for _ in 0..steps {
		let existing = |rng: &mut Rng, w: &World| {
			if w.entries.is_empty() { s.root.join("nothing") } else { rng.pick(&w.entries).clone() }
		};
		// A name not taken yet, as far as the generator knows (a few tries: collisions are wanted too).
		let fresh = |rng: &mut Rng, w: &World| {
			let mut path = PathBuf::new();
			for _ in 0..4 {
				let dir = if w.dirs.is_empty() { s.root.clone() } else { rng.pick(&w.dirs).clone() };
				path = dir.join(rng.pick(NEW_NAMES));
				if !w.entries.contains(&path) && fs::symlink_metadata(&path).is_err() {
					break;
				}
			}
			path
		};
		let any = |rng: &mut Rng, w: &World| match rng.below(10) {
			0..7 => existing(rng, w),
			7..9 => fresh(rng, w),
			_ => existing(rng, w).join(rng.pick(NAMES)),
		};
		let twist = |rng: &mut Rng, w: &World, p: PathBuf| {
			if hostile && rng.chance(20) { hostile_path(rng, s, w) } else { p }
		};
		let source = if rng.chance(88) { existing(rng, w) } else { any(rng, w) };
		let source = twist(rng, w, source);
		let mut target = if rng.chance(93) { fresh(rng, w) } else { any(rng, w) };
		// Seldom into the source itself.
		for _ in 0..3 {
			if target.starts_with(&source) {
				target = fresh(rng, w);
			}
		}
		let target = twist(rng, w, target);
		let dir = w.is_dir(&source);
		let op = match rng.below(100) {
			0..25 => {
				w.forget(&source);
				w.add(&target, dir);
				Op::Move { from: source, to: target }
			}
			25..45 => {
				w.add(&target, dir);
				Op::Copy { from: source, to: target }
			}
			45..60 => {
				let own = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
				let new_name = match rng.below(if hostile { 12 } else { 8 }) {
					0 => own.to_uppercase(),
					1 => own.to_lowercase(),
					2 => own,
					3..8 => (*rng.pick(NEW_NAMES)).to_owned(),
					8 => "a/b".to_owned(),
					9 => "..".to_owned(),
					10 => String::new(),
					_ => ".tpfs-case-0123abcd-0".to_owned(),
				};
				w.forget(&source);
				w.add(&source.with_file_name(&new_name), dir);
				Op::Rename { path: source, new_name }
			}
			60..75 => {
				w.forget(&source);
				Op::Trash { path: source }
			}
			75..90 => {
				w.add(&target, true);
				Op::Mkdir { path: target }
			}
			_ => {
				// Mostly a folder the plan so far left empty.
				let empty: Vec<&PathBuf> = w
					.dirs
					.iter()
					.filter(|d| !s.roots().contains(d) && !w.entries.iter().any(|e| e.parent() == Some(d.as_path())))
					.collect();
				let path = if !empty.is_empty() && rng.chance(85) { (*rng.pick(&empty)).clone() } else { source };
				let none_empty = empty.is_empty();
				w.forget(&path);
				if none_empty && rng.chance(60) { Op::Trash { path } } else { Op::Rmdir { path } }
			}
		};
		ops.push(op);
	}
	Plan::new(ops, if rng.chance(3) { "r".repeat(5000) } else { "randomized plan".to_owned() })
}

#[derive(Default, Debug)]
struct Tally {
	refused: u64,
	failed: u64,
	committed: u64,
	crashed: u64,
}

/// Prints how to run the case again if it fails.
struct Case {
	seed: u64,
	plan: String,
}

impl Drop for Case {
	fn drop(&mut self) {
		if std::thread::panicking() {
			eprintln!(
				"\nrandomized plan failed. Run it again alone with:\n  TPFS_FUZZ_SEED={} TPFS_FUZZ_CASES=1 cargo test -p tpfs-core --test fuzz_plans\n{}\n",
				self.seed, self.plan
			);
		}
	}
}

fn run_case(seed: u64, tally: &mut Tally) {
	let mut case = Case { seed, plan: String::new() };
	let mut rng = Rng::new(seed);
	// One case in five spans two file systems, where there are two.
	let s = if rng.chance(20) { Sandbox::two_devices().unwrap_or_else(Sandbox::new) } else { Sandbox::new() };
	let mut world = random_tree(&mut rng, &s);
	let before = s.snapshot();
	let plan = random_plan(&mut rng, &s, &mut world);
	case.plan = format!("tree: {before:#?}\nplan: {plan:#?}");

	// None of these may panic, whatever the plan says.
	let issues = s.validator().validate(&plan);
	let _ = (plan.preview(), plan.digest(), plan.normalized());

	// Early points more often than late ones: most plans are short.
	let crash_point = |rng: &mut Rng, percent: usize| {
		rng.chance(percent).then(|| {
			let span = 1 + rng.below(60);
			rng.below(span)
		})
	};
	let crash_at = crash_point(&mut rng, 40);
	let (result, _) = s.execute(&plan, crash_at, &|_| {});
	match result {
		Err(crashed) => {
			tally.crashed += 1;
			let what = format!("crash at #{} ({})", crashed.index, crashed.at);
			if let Some(k) = crash_point(&mut rng, 40)
				&& s.recover(Some(k)).0.is_ok()
			{
				assert_tree(&s, &before, &format!("{what}, recovered"));
			}
			let report = s.recover(None).0.unwrap();
			assert!(report.errors.is_empty(), "{what}: recovery reported {:?}", report.errors);
			assert_tree(&s, &before, &format!("{what}, recovered"));
		}
		Ok(Err(ExecError::Rejected(found))) => {
			tally.refused += 1;
			assert!(!found.is_empty());
			assert_tree(&s, &before, "refused");
			assert!(s.statuses().is_empty(), "a refused plan reached the journal");
		}
		Ok(Err(ExecError::Failed { step, error, rollback_errors })) => {
			tally.failed += 1;
			let what = format!("step #{} failed ({error})", step + 1);
			assert!(issues.is_empty(), "{what}, though validation had refused the plan: {issues:?}");
			assert!(rollback_errors.is_empty(), "{what}: rollback was incomplete: {rollback_errors:?}");
			assert_tree(&s, &before, &what);
		}
		Ok(Err(e)) => panic!("unexpected: {e}"),
		Ok(Ok(report)) => {
			tally.committed += 1;
			assert!(issues.is_empty(), "committed, though validation had refused the plan: {issues:?}");
			assert_eq!(report.steps, plan.ops.len());
			let mut what = "undo".to_owned();
			let mut undone = s.undo(crash_point(&mut rng, 40)).0;
			if let Err(crashed) = undone {
				what = format!("undo crashed at #{} ({}), undone again", crashed.index, crashed.at);
				undone = s.undo(None).0;
			}
			assert!(matches!(undone, Ok(Ok(Some(_)))), "{what}: {undone:?}");
			assert_tree(&s, &before, &what);
		}
	}
	// And the engine is in working order: nothing is left to recover or to undo twice.
	let report = s.recover(None).0.unwrap();
	assert!(report.rolled_back.is_empty() && report.errors.is_empty(), "left to recover: {report:?}");
	assert_tree(&s, &before, "a recovery with nothing to do");
}

fn env_number(name: &str, default: u64) -> u64 {
	match std::env::var(name) {
		Ok(v) => v.parse().unwrap_or_else(|_| panic!("{name} must be a number, got {v:?}")),
		Err(_) => default,
	}
}

#[test]
fn randomized_plans_keep_every_promise() {
	let cases = env_number("TPFS_FUZZ_CASES", CASES);
	let seed = env_number("TPFS_FUZZ_SEED", SEED);
	let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8) as u64;
	let workers: Vec<_> = (0..threads)
		.map(|t| {
			std::thread::spawn(move || {
				let mut tally = Tally::default();
				for k in (t..cases).step_by(threads as usize) {
					run_case(seed.wrapping_add(k), &mut tally);
				}
				tally
			})
		})
		.collect();
	let mut total = Tally::default();
	let mut failed = false;
	for w in workers {
		match w.join() {
			Ok(t) => {
				total.refused += t.refused;
				total.failed += t.failed;
				total.committed += t.committed;
				total.crashed += t.crashed;
			}
			Err(_) => failed = true,
		}
	}
	assert!(!failed, "a randomized plan failed: see the seed printed above");
	eprintln!("{cases} randomized plans from seed {seed}: {total:?}");
	// The generator still produces plans that run: a harness that only sees refusals tests nothing.
	if cases >= 100 {
		assert!(total.committed + total.crashed >= cases / 5, "too few plans were accepted: {total:?}");
	}
}
