use std::{
	collections::{HashMap, HashSet},
	fmt, fs, io,
	path::{Path, PathBuf},
};

use crate::{
	display::sanitize,
	fsops::same_file,
	op::{Op, Plan},
	path::{exists, normalize, symlink_in_between},
};

/// Upper bound on operations in one plan.
pub const MAX_OPS: usize = 10_000;

/// A problem found in a plan. `index` points at the offending operation (0-based), if any.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
	pub index: Option<usize>,
	pub message: String,
}

impl fmt::Display for Issue {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self.index {
			Some(i) => write!(f, "#{}: {}", i + 1, self.message),
			None => f.write_str(&self.message),
		}
	}
}

/// Checks a plan before anything touches the disk.
///
/// Every path must lie inside an allowed root (lexically normalised, roots canonicalised) and must
/// not pass through a symlink below the root. Sources must exist, targets must not, and the
/// operations are simulated in order, so "mkdir X, then move a into X" is valid while
/// "move a twice" is not.
#[derive(Clone, Debug)]
pub struct Validator {
	roots: Vec<PathBuf>,
	protected: Vec<PathBuf>,
	max_ops: usize,
}

/// Names tpfs uses for its own work inside user folders: local trash, partial copies, case renames.
const OWN_PREFIXES: [&str; 3] = [".tpfs-trash-", ".tpfs-part-", ".tpfs-case-"];

/// Whether a path name is one of tpfs's own work names.
pub(crate) fn is_own_name(name: &std::ffi::OsStr) -> bool {
	name.to_str().is_some_and(|n| OWN_PREFIXES.iter().any(|p| n.starts_with(p)))
}

impl Validator {
	/// Roots are canonicalised (symlinks resolved), so a root chosen by the user is taken as it really is.
	pub fn new<I, P>(roots: I) -> io::Result<Self>
	where
		I: IntoIterator<Item = P>,
		P: AsRef<Path>,
	{
		let mut out = Vec::new();
		for r in roots {
			let r = crate::path::canonicalize(r.as_ref())?;
			if !r.is_dir() {
				return Err(io::Error::new(io::ErrorKind::NotADirectory, format!("not a directory: {}", r.display())));
			}
			if !out.contains(&r) {
				out.push(r);
			}
		}
		if out.is_empty() {
			return Err(io::Error::new(io::ErrorKind::InvalidInput, "at least one allowed root is required"));
		}
		Ok(Self { roots: out, protected: Vec::new(), max_ops: MAX_OPS })
	}

	/// Folders no plan may touch, read or contain: tpfs's state directory (journal, trash, pending
	/// approvals). Without this an agent whose root contains the state could forge the journal or
	/// approve its own plan.
	pub fn with_protected<I, P>(mut self, paths: I) -> Self
	where
		I: IntoIterator<Item = P>,
		P: AsRef<Path>,
	{
		for p in paths {
			let p = p.as_ref();
			// Both spellings: the one plans will use (canonical) and the one given.
			for q in [crate::path::canonicalize(p).ok(), normalize(p)].into_iter().flatten() {
				if !self.protected.contains(&q) {
					self.protected.push(q);
				}
			}
		}
		self
	}

	pub fn protected(&self) -> &[PathBuf] {
		&self.protected
	}

	/// Raise (or lower) the operation limit, for a plan the user built on purpose, such as a photo
	/// library clean-up with tens of thousands of moves. Agent plans keep the default.
	pub fn with_max_ops(mut self, max_ops: usize) -> Self {
		self.max_ops = max_ops;
		self
	}

	pub fn roots(&self) -> &[PathBuf] {
		&self.roots
	}

	/// A source that would carry tpfs's state with it (moving, copying or trashing a folder that
	/// contains the state directory).
	fn holds_state(&self, path: &Path) -> Result<(), String> {
		match self.protected.iter().find(|p| p.starts_with(path)) {
			Some(p) => Err(format!("{} contains tpfs's state ({}); leave it out of the plan.", show(path), show(p))),
			None => Ok(()),
		}
	}

	fn root_of(&self, path: &Path) -> Option<&Path> {
		self.roots.iter().filter(|r| path.starts_with(r)).max_by_key(|r| r.as_os_str().len()).map(PathBuf::as_path)
	}

	fn is_root(&self, path: &Path) -> bool {
		self.roots.iter().any(|r| r == path)
	}

	/// Normalise a single path and check it is inside a root and not reached through a symlink.
	pub fn check_path(&self, path: &Path) -> Result<PathBuf, String> {
		self.resolve(path)
	}

	pub fn validate(&self, plan: &Plan) -> Vec<Issue> {
		if plan.ops.is_empty() {
			let message = if plan.reason.trim().is_empty() {
				"The plan is empty.".to_owned()
			} else {
				format!("The plan is empty: {}", plan.reason)
			};
			return vec![Issue { index: None, message }];
		}
		if plan.ops.len() > self.max_ops {
			return vec![Issue {
				index: None,
				message: format!("Too many operations ({}, limit {}).", plan.ops.len(), self.max_ops),
			}];
		}

		let mut sim = Sim::default();
		plan.ops
			.iter()
			.enumerate()
			.filter_map(|(i, op)| self.check(op, &mut sim).err().map(|message| Issue { index: Some(i), message }))
			.collect()
	}

	fn check(&self, op: &Op, sim: &mut Sim) -> Result<(), String> {
		match op {
			Op::Move { from, to } => self.transfer(from, to, true, sim),
			Op::Copy { from, to } => self.transfer(from, to, false, sim),
			Op::Rename { path, new_name } => {
				if new_name.is_empty() || new_name == "." || new_name == ".." || new_name.contains(['/', '\\', '\0']) {
					return Err(format!("Invalid file name: '{}'", sanitize(new_name.as_ref())));
				}
				let source = self.resolve(path)?;
				self.holds_state(&source)?;
				let parent = source.parent().ok_or("Cannot rename a file system root.")?;
				let target = parent.join(new_name);
				if target != source && same_file(&source, &target) && !sim.state.contains_key(&target) {
					// Only the letter case differs, on a case-insensitive file system.
					sim.set(source, false);
					sim.set(target, true);
					return Ok(());
				}
				self.transfer(&source, &target, true, sim)
			}
			Op::Trash { path } => {
				let path = self.resolve(path)?;
				if self.is_root(&path) {
					return Err("Cannot trash an allowed root folder.".into());
				}
				self.holds_state(&path)?;
				if !sim.exists(&path) {
					return Err(format!("Does not exist: {}", show(&path)));
				}
				sim.set(path, false);
				Ok(())
			}
			Op::Mkdir { path } => {
				let path = self.resolve(path)?;
				if sim.exists(&path) {
					return Err(format!("Already exists: {}", show(&path)));
				}
				let parent = path.parent().ok_or("Cannot create a file system root.")?;
				if !sim.exists(parent) {
					return Err(format!("Parent folder does not exist: {}", show(parent)));
				}
				sim.created_dirs.insert(path.clone());
				sim.set(path, true);
				Ok(())
			}
			Op::Rmdir { path } => {
				let path = self.resolve(path)?;
				if self.is_root(&path) {
					return Err("Cannot remove an allowed root folder.".into());
				}
				let on_disk = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
				if !sim.exists(&path) || !(on_disk || sim.created_dirs.contains(&path)) {
					return Err(format!("Not an existing directory: {}", show(&path)));
				}
				if !sim.is_empty_dir(&path) {
					return Err(format!("Directory is not empty: {}", show(&path)));
				}
				sim.set(path, false);
				Ok(())
			}
		}
	}

	fn transfer(&self, from: &Path, to: &Path, is_move: bool, sim: &mut Sim) -> Result<(), String> {
		let from = self.resolve(from)?;
		let to = self.resolve(to)?;
		self.holds_state(&from)?;

		if !sim.exists(&from) {
			return Err(format!("Source does not exist: {}", show(&from)));
		}
		if sim.exists(&to) {
			return Err(format!("Target already exists: {}", show(&to)));
		}
		let parent = to.parent().ok_or("Target cannot be a file system root.")?;
		if !sim.exists(parent) {
			return Err(format!("Target folder does not exist: {}", show(parent)));
		}
		if to.starts_with(&from) {
			return Err("Cannot put a folder inside itself.".into());
		}
		if is_move && self.is_root(&from) {
			return Err("Cannot move an allowed root folder.".into());
		}
		if !is_move {
			check_copyable(&from)?;
		}
		if !is_move || crosses_devices(&from, parent) {
			sim.reserve_space(&from, parent)?;
		}

		sim.set(to, true);
		if is_move {
			sim.set(from, false);
		}
		Ok(())
	}

	/// Normalise, confine to a root, and refuse paths that pass through a symlink.
	fn resolve(&self, path: &Path) -> Result<PathBuf, String> {
		let path = normalize(path).ok_or_else(|| format!("Path must be absolute: {}", show(path)))?;
		if path.to_str().is_none() {
			// The plan, its digest and the journal store paths as JSON strings.
			return Err(format!("File names that are not valid Unicode are not supported yet: {}", show(&path)));
		}
		if path.components().any(|c| is_own_name(c.as_os_str())) {
			return Err(format!("This is tpfs's own working data and cannot be changed by a plan: {}", show(&path)));
		}
		if self.protected.iter().any(|p| path.starts_with(p)) {
			return Err(format!(
				"This is tpfs's state (journal, trash, approvals) and cannot be part of a plan: {}",
				show(&path)
			));
		}
		let root = self.root_of(&path).ok_or_else(|| format!("Outside the allowed folders: {}", show(&path)))?;
		match symlink_in_between(&path, root) {
			Ok(None) => Ok(path),
			Ok(Some(link)) => Err(format!("Path goes through a symlink: {}", show(&link))),
			Err(e) => Err(format!("Cannot inspect {}: {e}", show(&path))),
		}
	}
}

/// FIFOs, sockets and device nodes are never copied: reading them can block forever or read a whole disk.
fn check_copyable(path: &Path) -> Result<(), String> {
	let Ok(meta) = fs::symlink_metadata(path) else { return Ok(()) };
	let ft = meta.file_type();
	if ft.is_file() || ft.is_dir() || ft.is_symlink() {
		Ok(())
	} else {
		Err(format!("Special files (FIFO, socket, device) cannot be copied: {}", show(path)))
	}
}

fn show(p: &Path) -> String {
	sanitize(p.as_os_str())
}

/// Simulated existence: path -> exists after the operations processed so far,
/// plus the bytes the plan will write per target file system.
#[derive(Default)]
struct Sim {
	state: HashMap<PathBuf, bool>,
	created_dirs: HashSet<PathBuf>,
	reserved: HashMap<u64, u64>,
}

impl Sim {
	fn exists(&self, p: &Path) -> bool {
		// A path removed earlier in the plan takes everything below it with it.
		for a in p.ancestors() {
			if let Some(&e) = self.state.get(a) {
				if a == p || !e {
					return e;
				}
				break;
			}
		}
		exists(p)
	}

	fn set(&mut self, p: PathBuf, exists: bool) {
		self.state.insert(p, exists);
	}

	/// Empty after the operations so far: no entry on disk that still exists, none added by the plan.
	fn is_empty_dir(&self, dir: &Path) -> bool {
		let on_disk =
			fs::read_dir(dir).map(|rd| rd.filter_map(Result::ok).any(|e| self.exists(&e.path()))).unwrap_or(false);
		let added = self.state.iter().any(|(p, &e)| e && p.parent() == Some(dir));
		!on_disk && !added
	}

	/// Refuse a copy that would not fit, counting everything the plan already writes to that file system.
	fn reserve_space(&mut self, from: &Path, target_dir: &Path) -> Result<(), String> {
		let Some((dev, free)) = free_space(target_dir) else { return Ok(()) };
		let needed = tree_size(from);
		let total = self.reserved.entry(dev).or_default();
		*total = total.saturating_add(needed);
		if *total > free {
			return Err(format!(
				"Not enough space in {}: the plan needs {} more, {} is free",
				show(target_dir),
				human(*total),
				human(free)
			));
		}
		Ok(())
	}
}

/// Logical size of a file or directory tree (symlinks count as nothing).
fn tree_size(path: &Path) -> u64 {
	let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
	if meta.is_dir() {
		fs::read_dir(path).map(|rd| rd.filter_map(Result::ok).map(|e| tree_size(&e.path())).sum()).unwrap_or(0)
	} else if meta.is_file() {
		meta.len()
	} else {
		0
	}
}

#[cfg(target_os = "linux")]
fn free_space(dir: &Path) -> Option<(u64, u64)> {
	use std::os::unix::fs::MetadataExt;
	let existing = dir.ancestors().find(|a| a.exists())?;
	let dev = fs::metadata(existing).ok()?.dev();
	let st = rustix::fs::statvfs(existing).ok()?;
	Some((dev, st.f_bavail.saturating_mul(st.f_frsize)))
}

#[cfg(not(target_os = "linux"))]
fn free_space(_: &Path) -> Option<(u64, u64)> {
	None
}

#[cfg(unix)]
fn crosses_devices(from: &Path, target_dir: &Path) -> bool {
	use std::os::unix::fs::MetadataExt;
	let dev = |p: &Path| p.ancestors().find_map(|a| fs::symlink_metadata(a).ok()).map(|m| m.dev());
	dev(from) != dev(target_dir)
}

#[cfg(not(unix))]
fn crosses_devices(_: &Path, _: &Path) -> bool {
	false
}

fn human(bytes: u64) -> String {
	const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
	let mut value = bytes as f64;
	let mut unit = 0;
	while value >= 1024.0 && unit < UNITS.len() - 1 {
		value /= 1024.0;
		unit += 1;
	}
	if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}
