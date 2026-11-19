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
/// Most folders a path in a plan may go through.
pub const MAX_DEPTH: usize = 256;

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

/// Whether a path name is one of tpfs's own work names: `.tpfs-trash-<uid>`, or a partial copy or
/// case-rename name (`.tpfs-part-<batch>-…`, `.tpfs-case-<batch>-…`). A user's `.tpfs-notes.txt`
/// is not one.
pub fn is_own_name(name: &std::ffi::OsStr) -> bool {
	// Letter case ignored: on a volume that ignores it (FAT, exFAT, APFS, NTFS, casefold folders)
	// `.TPFS-TRASH-1000` is the same folder as tpfs's trash.
	let Some(n) = name.to_str().map(fold) else { return false };
	let n = n.as_str();
	if let Some(uid) = n.strip_prefix(OWN_PREFIXES[0]) {
		return !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit());
	}
	OWN_PREFIXES[1..].iter().any(|p| {
		n.strip_prefix(p).is_some_and(|rest| {
			rest.split('-').next().is_some_and(|b| b.len() >= 8 && b.bytes().all(|c| c.is_ascii_hexdigit()))
		})
	})
}

/// A name the way the lenient volumes compare it, for checks that must not be dodged by another
/// spelling of tpfs's own names: lower case; the letters whose case folding is plain ASCII (`ſ`,
/// the Kelvin sign, the `ﬀ`…`ﬆ` ligatures) folded; and trailing dots and spaces dropped, which
/// Windows and the FAT/exFAT drivers ignore. Not full Unicode folding or normalisation: the
/// protected names themselves are ASCII, apart from a user's own folder names.
fn fold(name: &str) -> String {
	let mut out = String::with_capacity(name.len());
	for c in name.chars().flat_map(char::to_lowercase) {
		match c {
			'\u{17f}' => out.push('s'),
			'\u{fb00}' => out.push_str("ff"),
			'\u{fb01}' => out.push_str("fi"),
			'\u{fb02}' => out.push_str("fl"),
			'\u{fb03}' => out.push_str("ffi"),
			'\u{fb04}' => out.push_str("ffl"),
			'\u{fb05}' | '\u{fb06}' => out.push_str("st"),
			c => out.push(c),
		}
	}
	let kept = out.trim_end_matches(['.', ' ']).len();
	// "." and ".." stay what they are.
	if kept > 0 && !matches!(out.as_str(), "." | "..") {
		out.truncate(kept);
	}
	out
}

/// Whether `path` is `base` or inside it, letter case ignored: on a volume that ignores case,
/// `/data/.TPFS` is the state folder `/data/.tpfs`. Over-matching on a case-sensitive volume only
/// refuses more.
fn within(path: &Path, base: &Path) -> bool {
	let (mut p, mut b) = (path.components(), base.components());
	loop {
		match (b.next(), p.next()) {
			(None, _) => return true,
			(Some(_), None) => return false,
			(Some(x), Some(y)) => {
				let (x, y) = (x.as_os_str().to_string_lossy(), y.as_os_str().to_string_lossy());
				if x != y && fold(&x) != fold(&y) {
					return false;
				}
			}
		}
	}
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
		match self.protected.iter().find(|p| within(p, path)) {
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
		if plan.reason.chars().count() > MAX_REASON {
			return vec![Issue {
				index: None,
				message: format!(
					"The reason is too long ({} characters, limit {MAX_REASON}).",
					plan.reason.chars().count()
				),
			}];
		}
		if plan.ops.is_empty() {
			let message = if plan.reason.trim().is_empty() {
				"The plan is empty.".to_owned()
			} else {
				format!("The plan is empty: it has no operations (reason: {})", sanitize(plan.reason.as_ref()))
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
				// On a case-insensitive file system the new spelling is not listed as an entry of its
				// own; a hard link under that spelling (case-sensitive file systems) is.
				let mut own_entry = || {
					sim.names
						.entry(parent.to_path_buf())
						.or_insert_with(|| {
							fs::read_dir(parent)
								.map(|rd| rd.filter_map(Result::ok).map(|e| e.file_name()).collect())
								.unwrap_or_default()
						})
						.contains(std::ffi::OsStr::new(new_name))
				};
				let old_name = source.file_name().map(|n| fold(&n.to_string_lossy()));
				let case_only = target != source && old_name == Some(fold(new_name));
				// Cheap checks first; the folder is read only for an entry with several names.
				let alias = case_only && same_file(&source, &target) && !sim.state.contains_key(&sim_key(&target));
				let fold = alias && (crate::fsops::link_count(&source) == Some(1) || !own_entry());
				if fold {
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
				refuse_mount_point(&path)?;
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
				sim.created_dirs.insert(sim_key(&path));
				sim.set(path, true);
				Ok(())
			}
			Op::Rmdir { path } => {
				let path = self.resolve(path)?;
				if self.is_root(&path) {
					return Err("Cannot remove an allowed root folder.".into());
				}
				let on_disk = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
				if !sim.exists(&path) || !(on_disk || sim.created_dirs.contains(&sim_key(&path))) {
					return Err(format!("Not an existing directory: {}", show(&path)));
				}
				if !sim.is_empty_dir(&path) {
					return Err(format!("Directory is not empty: {}", show(&path)));
				}
				// An empty mounted disk is not an empty folder (its undo could not mount it back).
				if on_disk {
					refuse_mount_point(&path)?;
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
		if is_move {
			refuse_mount_point(&from)?;
		}

		if !sim.exists(&from) {
			return Err(format!("Source does not exist: {}", show(&from)));
		}
		if sim.exists(&to) {
			// The usual slip: naming the destination folder instead of the full new path.
			let hint = match (to.is_dir(), from.file_name()) {
				(true, Some(name)) => {
					format!(" ('to' is the full new path: to put it inside that folder, use {})", show(&to.join(name)))
				}
				_ => String::new(),
			};
			return Err(format!("Target already exists: {}{hint}", show(&to)));
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
		if !is_move || crate::path::crosses_devices(&from, parent) {
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
		// Each check below looks at every folder on the path: a bound on their number keeps a plan
		// of very deep paths from taking minutes to validate.
		if path.components().count() > MAX_DEPTH {
			return Err(format!("Path is more than {MAX_DEPTH} folders deep."));
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
		if self.protected.iter().any(|p| within(&path, p)) {
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
				&& stem[3..].chars().all(|d| d.is_ascii_digit() || "¹²³".contains(d)));
		// Characters Windows refuses in names (`:` would also open an alternate data stream).
		// `/` inside a component only happens in a verbatim (`\\?\`) path, where it is not a
		// separator — but would become one once the prefix is dropped.
		let forbidden = name.chars().any(|c| matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*' | '/') || c < ' ');
		if forbidden || name.ends_with(['.', ' ']) || device {
			return Err(format!("Windows cannot use this name safely: '{}'", sanitize(name.as_ref().as_ref())));
		}
	}
	Ok(())
}

/// A folder where another file system is mounted cannot be moved or trashed (the kernel refuses,
/// and copying it into a trash on its own volume would copy it into itself): refused up front.
fn refuse_mount_point(path: &Path) -> Result<(), String> {
	if is_mount_point(path) {
		return Err(format!(
			"{} is a mount point or a separate file system (another disk, a share, a subvolume); work on what is inside it",
			show(path)
		));
	}
	Ok(())
}

#[cfg(test)]
thread_local! {
	/// A path the tests treat as another disk mounted there.
	pub(crate) static FAKE_MOUNT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Whether a folder is the root of a mount: on Linux as the kernel says (a bind mount has the same
/// device, a btrfs subvolume a different one without being a mount point), on other Unix systems
/// by a device different from its parent's. Always false on Windows, where mounted volumes are
/// reparse points and refused as links.
pub(crate) fn is_mount_point(path: &Path) -> bool {
	#[cfg(test)]
	if FAKE_MOUNT.with(|f| f.borrow().as_deref() == Some(path)) {
		return true;
	}
	#[cfg(target_os = "linux")]
	if let Ok(st) = rustix::fs::statx(
		rustix::fs::CWD,
		path,
		rustix::fs::AtFlags::NO_AUTOMOUNT | rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
		rustix::fs::StatxFlags::empty(),
	) && st.stx_attributes_mask.contains(rustix::fs::StatxAttributes::MOUNT_ROOT)
	{
		return st.stx_attributes.contains(rustix::fs::StatxAttributes::MOUNT_ROOT);
	}
	#[cfg(unix)]
	if let Some(parent) = path.parent() {
		use std::os::unix::fs::MetadataExt;
		if let (Ok(m), Ok(p)) = (fs::symlink_metadata(path), fs::symlink_metadata(parent)) {
			return m.is_dir() && m.dev() != p.dev();
		}
	}
	let _ = path;
	false
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
	/// Entries left to measure for the whole plan: past it, sizes are not checked (the executor
	/// still stops cleanly when a disk fills up).
	size_budget: Option<usize>,
	/// Exact entry names of folders read for the case-only rename check, one read per folder.
	names: HashMap<PathBuf, HashSet<std::ffi::OsString>>,
}

/// Windows and macOS (by default) treat names that differ only in letter case as one: so does the
/// simulator, or two new targets `new.txt` and `NEW.txt` would both pass and the second would fail
/// half way through the plan.
fn sim_key(p: &Path) -> PathBuf {
	if cfg!(any(windows, target_vendor = "apple")) {
		PathBuf::from(p.as_os_str().to_string_lossy().to_lowercase())
	} else {
		p.to_path_buf()
	}
}

impl Sim {
	fn exists(&self, p: &Path) -> bool {
		// A path removed earlier in the plan takes everything below it with it.
		for a in p.ancestors() {
			if let Some(&e) = self.state.get(&sim_key(a)) {
				if a == p || !e {
					return e;
				}
				break;
			}
		}
		exists(p)
	}

	fn set(&mut self, p: PathBuf, exists: bool) {
		let p = sim_key(&p);
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
			return !exists(dir) && self.added.get(&sim_key(dir)).is_none_or(HashSet::is_empty);
		};
		let on_disk = rd.into_iter().any(|e| e.map_or(true, |e| self.exists(&e.path())));
		!on_disk && self.added.get(&sim_key(dir)).is_none_or(HashSet::is_empty)
	}

	/// Refuse a copy that would not fit, counting everything the plan already writes to that file system.
	fn reserve_space(&mut self, from: &Path, target_dir: &Path) -> Result<(), String> {
		let Some((dev, free)) = free_space(target_dir) else { return Ok(()) };
		let needed = match self.sizes.get(from) {
			Some(&n) => n,
			None => {
				let budget = self.size_budget.get_or_insert(1_000_000);
				let Some(n) = tree_size(from, budget) else { return Ok(()) };
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

/// Logical size of a file or directory tree (symlinks count as nothing), within a budget of
/// entries shared by the whole plan; `None` once it runs out.
fn tree_size(path: &Path, budget: &mut usize) -> Option<u64> {
	let mut total = 0u64;
	let mut stack = vec![path.to_path_buf()];
	while let Some(p) = stack.pop() {
		*budget = budget.checked_sub(1)?;
		let Ok(meta) = fs::symlink_metadata(&p) else { continue };
		if meta.is_dir() {
			if let Ok(rd) = fs::read_dir(&p) {
				// Never more pending paths in memory than the budget can still measure.
				for e in rd.filter_map(Result::ok) {
					if stack.len() >= *budget {
						return None;
					}
					stack.push(e.path());
				}
			}
		} else if meta.is_file() {
			total = total.saturating_add(meta.len());
		}
	}
	Some(total)
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
	(ok != 0).then(|| (crate::path::device_of(existing).unwrap_or(0), free))
}

#[cfg(not(any(unix, windows)))]
fn free_space(_: &Path) -> Option<(u64, u64)> {
	None
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

#[cfg(test)]
mod tests {
	use super::*;

	/// A tree with more entries than the plan's budget is not measured: `None`. A deep, narrow
	/// chain never holds more than one pending path, so only the entry budget can stop it.
	#[test]
	fn tree_size_stops_at_the_budget() {
		let tmp = tempfile::TempDir::new().unwrap();
		let mut deep = tmp.path().join("chain");
		for _ in 0..30 {
			deep = deep.join("d");
		}
		fs::create_dir_all(&deep).unwrap();
		let chain = tmp.path().join("chain");
		assert_eq!(tree_size(&chain, &mut 10), None);
		let mut budget = 100;
		assert_eq!(tree_size(&chain, &mut budget), Some(0));
		assert_eq!(budget, 69);
	}

	/// A wide folder stops as soon as the pending paths would outgrow what the budget can still
	/// measure, before reading the rest.
	#[test]
	fn tree_size_holds_no_more_than_the_budget() {
		let tmp = tempfile::TempDir::new().unwrap();
		for i in 0..50 {
			fs::write(tmp.path().join(format!("f{i}")), "x").unwrap();
		}
		let mut budget = 10;
		assert_eq!(tree_size(tmp.path(), &mut budget), None);
		assert_eq!(budget, 9, "stopped while listing, before measuring any file");
		let mut budget = 100;
		assert_eq!(tree_size(tmp.path(), &mut budget), Some(50));
		assert_eq!(budget, 49);
	}

	/// A disk mounted at a plan's path cannot be trashed, moved or removed (on every OS, with a
	/// mount simulated).
	#[test]
	fn a_mount_point_is_refused_on_every_os() {
		let tmp = tempfile::TempDir::new().unwrap();
		let root = crate::path::canonicalize(tmp.path()).unwrap();
		let m = root.join("m");
		fs::create_dir(&m).unwrap();
		FAKE_MOUNT.with(|f| *f.borrow_mut() = Some(m.clone()));
		let v = Validator::new([&root]).unwrap();
		for op in [
			Op::Trash { path: m.clone() },
			Op::Move { from: m.clone(), to: root.join("x") },
			Op::Rmdir { path: m.clone() },
		] {
			let issues = v.validate(&crate::Plan::new(vec![op.clone()], ""));
			assert!(issues.iter().any(|i| i.message.contains("mount point")), "{op}: {issues:?}");
		}
	}
}
