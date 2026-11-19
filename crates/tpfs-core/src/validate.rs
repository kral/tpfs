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

/// Longest plan reason, in characters. The reason is journaled with the batch, and every journal
/// record must stay far below the journal's line limit.
pub const MAX_REASON: usize = 10_000;

/// Longest path accepted, in bytes (PATH_MAX on Linux; Windows long paths fit too).
pub const MAX_PATH: usize = 4096;

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
		if plan.reason.chars().count() > MAX_REASON {
			return vec![Issue {
				index: None,
				message: format!(
					"The reason is too long ({} characters, limit {MAX_REASON}).",
					plan.reason.chars().count()
				),
			}];
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
				let case_only =
					source.file_name().map(|n| n.to_string_lossy().to_lowercase()) == Some(new_name.to_lowercase());
				if target != source && case_only && same_file(&source, &target) && !sim.state.contains_key(&target) {
					// Only the letter case differs, on a case-insensitive file system (a hard link
					// under another name is a different entry, and goes the normal way).
					check_names(&target)?;
					if !sim.exists(&source) {
						return Err(format!("Source does not exist: {}", show(&source)));
					}
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
	///
	/// The result is the path as the file system spells it (existing part canonicalised), so that
	/// different spellings of one folder — `Photos` and `photos` on a case-insensitive file system,
	/// `Photos.` or an 8.3 short name on Windows — are recognised as the same root.
	fn resolve(&self, path: &Path) -> Result<PathBuf, String> {
		let path = normalize(path).ok_or_else(|| format!("Path must be absolute: {}", show(path)))?;
		if path.as_os_str().len() > MAX_PATH {
			return Err(format!("Path is longer than {MAX_PATH} bytes."));
		}
		if path.to_str().is_none() {
			// The plan, its digest and the journal store paths as JSON strings.
			return Err(format!("File names that are not valid Unicode are not supported yet: {}", show(&path)));
		}
		check_names(&path)?;
		// Symlinks are judged on the path as written…
		if let Some(root) = self.root_of(&path)
			&& let Ok(Some(link)) = symlink_in_between(&path, root)
		{
			return Err(format!("Path goes through a symlink: {}", show(&link)));
		}
		if self.root_of(&path).is_none() && through_symlink(&path) {
			return Err(format!("Outside the allowed folders: {}", show(&path)));
		}
		// …and the root on the path as the file system spells it.
		let path = spelled(&path);
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

/// Whether any existing directory on the way to `path` is a symlink.
fn through_symlink(path: &Path) -> bool {
	path.ancestors().skip(1).any(|a| fs::symlink_metadata(a).is_ok_and(|m| m.file_type().is_symlink()))
}

/// The file system's own spelling of `path`: its existing directories canonicalised, and the
/// last component too when it exists and is not a symlink (operations act on a link itself).
/// Symlinks in between are caught afterwards by comparing against the roots.
fn spelled(path: &Path) -> PathBuf {
	if let Ok(m) = fs::symlink_metadata(path)
		&& !m.file_type().is_symlink()
		&& let Ok(c) = crate::path::canonicalize(path)
	{
		return c;
	}
	let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return path.to_path_buf() };
	let mut rest = vec![name];
	let mut base = parent;
	loop {
		if let Ok(c) = crate::path::canonicalize(base) {
			let mut out = c;
			out.extend(rest.iter().rev());
			return out;
		}
		match (base.parent(), base.file_name()) {
			(Some(p), Some(n)) => {
				rest.push(n);
				base = p;
			}
			_ => return path.to_path_buf(),
		}
	}
}

/// Names Windows cannot use or silently changes: `a:b` (an alternate data stream of `a`),
/// trailing dots and spaces (stripped), and device names such as `CON` or `com1.txt`.
fn check_names(path: &Path) -> Result<(), String> {
	if !cfg!(windows) {
		return Ok(());
	}
	for c in path.components() {
		let std::path::Component::Normal(name) = c else { continue };
		let name = name.to_string_lossy();
		let stem = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
		let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
			|| ((stem.starts_with("COM") || stem.starts_with("LPT"))
				&& stem.chars().count() == 4
				&& stem[3..].chars().all(|d| ('1'..='9').contains(&d) || "¹²³".contains(d)));
		if name.contains(':') || name.ends_with(['.', ' ']) || device {
			return Err(format!("Windows cannot use this name safely: '{}'", sanitize(name.as_ref().as_ref())));
		}
	}
	Ok(())
}

/// FIFOs, sockets and device nodes are never copied: reading them can block forever or read a whole disk.
fn check_copyable(path: &Path) -> Result<(), String> {
	let Ok(meta) = fs::symlink_metadata(path) else { return Ok(()) };
	let ft = meta.file_type();
	if cfg!(windows) && ft.is_symlink() {
		// Creating symlinks needs a privilege on Windows; refuse up front, not half way.
		Err(format!("Copying a symlink or junction is not supported on Windows: {}", show(path)))
	} else if ft.is_file() || ft.is_dir() || ft.is_symlink() {
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
	/// Entries the plan makes exist, by parent folder (for `rmdir` checks in constant time).
	added: HashMap<PathBuf, HashSet<PathBuf>>,
	created_dirs: HashSet<PathBuf>,
	reserved: HashMap<u64, u64>,
	/// Tree sizes already measured in this plan.
	sizes: HashMap<PathBuf, u64>,
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
		if let Some(parent) = p.parent() {
			let children = self.added.entry(parent.to_path_buf()).or_default();
			if exists {
				children.insert(p.clone());
			} else {
				children.remove(&p);
			}
		}
		self.state.insert(p, exists);
	}

	/// Empty after the operations so far: no entry on disk that still exists, none added by the plan.
	/// A folder that cannot be read counts as not empty.
	fn is_empty_dir(&self, dir: &Path) -> bool {
		let Ok(rd) = fs::read_dir(dir) else {
			return !exists(dir) && self.added.get(dir).is_none_or(HashSet::is_empty);
		};
		let on_disk = rd.into_iter().any(|e| e.map_or(true, |e| self.exists(&e.path())));
		!on_disk && self.added.get(dir).is_none_or(HashSet::is_empty)
	}

	/// Refuse a copy that would not fit, counting everything the plan already writes to that file system.
	fn reserve_space(&mut self, from: &Path, target_dir: &Path) -> Result<(), String> {
		let Some((dev, free)) = free_space(target_dir) else { return Ok(()) };
		let needed = match self.sizes.get(from) {
			Some(&n) => n,
			None => {
				let n = tree_size(from);
				self.sizes.insert(from.to_path_buf(), n);
				n
			}
		};
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

/// (file system id, bytes free to the user) for the file system holding `dir`.
#[cfg(unix)]
fn free_space(dir: &Path) -> Option<(u64, u64)> {
	use std::os::unix::fs::MetadataExt;
	let existing = dir.ancestors().find(|a| a.exists())?;
	let dev = fs::metadata(existing).ok()?.dev();
	let st = rustix::fs::statvfs(existing).ok()?;
	Some((dev, st.f_bavail.saturating_mul(st.f_frsize)))
}

#[cfg(windows)]
fn free_space(dir: &Path) -> Option<(u64, u64)> {
	use std::os::windows::ffi::OsStrExt;
	use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
	let existing = dir.ancestors().find(|a| a.exists())?;
	let wide: Vec<u16> = crate::path::win_api_path(existing).encode_wide().chain(Some(0)).collect();
	let mut free: u64 = 0;
	// SAFETY: `wide` is NUL-terminated and outlives the call; the out-pointer is a valid u64 and the
	// two optional out-pointers are null.
	let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
	(ok != 0).then(|| (volume_key(existing), free))
}

#[cfg(not(any(unix, windows)))]
fn free_space(_: &Path) -> Option<(u64, u64)> {
	None
}

/// A key for the volume of a Windows path: its drive or share prefix.
#[cfg(windows)]
fn volume_key(p: &Path) -> u64 {
	use std::hash::{Hash, Hasher};
	let mut h = std::collections::hash_map::DefaultHasher::new();
	if let Some(std::path::Component::Prefix(pre)) = p.components().next() {
		pre.as_os_str().to_ascii_uppercase().hash(&mut h);
	}
	h.finish()
}

#[cfg(unix)]
fn crosses_devices(from: &Path, target_dir: &Path) -> bool {
	use std::os::unix::fs::MetadataExt;
	let dev = |p: &Path| p.ancestors().find_map(|a| fs::symlink_metadata(a).ok()).map(|m| m.dev());
	dev(from) != dev(target_dir)
}

#[cfg(windows)]
fn crosses_devices(from: &Path, target_dir: &Path) -> bool {
	volume_key(from) != volume_key(target_dir)
}

#[cfg(not(any(unix, windows)))]
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
