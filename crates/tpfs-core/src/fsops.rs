//! File operations that never overwrite anything.
//!
//! Paths inside the allowed roots are not resolved by plain path: each operation runs relative to a
//! handle of its parent folder, opened from the root without following any link — `openat2(RESOLVE_BENEATH
//! | RESOLVE_NO_SYMLINKS)` on Linux, `O_NOFOLLOW_ANY` on macOS 11+, and on Windows a chain of
//! `NtCreateFile` handles (`FILE_OPEN_REPARSE_POINT`, links refused, held without `FILE_SHARE_DELETE`)
//! with renames and deletes by handle. A link swapped into the path after validation therefore makes
//! the operation fail instead of redirecting it. A root that cannot be anchored this way is refused
//! (fail closed) unless the user sets `TPFS_ALLOW_UNCONFINED=1`. Paths outside the roots (the private
//! trash) use plain path-based calls.

use std::{
	fs::{self, File, OpenOptions},
	io,
	path::{Path, PathBuf},
};

/// One directory entry, as listed by [`Fs::list_entries`].
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct Listed {
	pub name: std::ffi::OsString,
	pub dir: bool,
	pub link: bool,
	pub size: u64,
}

/// List `dir`, which must be below one of `roots`, without following any link on the way: the
/// listing and each entry's kind and size are read relative to the directory's own confined handle.
/// At most `max` entries are returned (memory stays bounded); the second value counts them all.
/// A `dir` outside every root is refused.
pub fn list_confined(roots: &[PathBuf], dir: &Path, max: usize) -> io::Result<(Vec<Listed>, usize)> {
	if !roots.iter().any(|r| dir.starts_with(r)) {
		return Err(io::Error::new(
			io::ErrorKind::PermissionDenied,
			format!("refused: {} is outside the allowed folders", crate::sanitize(dir.as_os_str())),
		));
	}
	Fs::confined(roots).list_entries(dir, true, max)
}

pub(crate) struct Fs {
	#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
	anchors: Vec<confined::Anchor>,
	/// Roots that have no anchor (it could not be opened, or this platform has none): operations
	/// below them are refused, or — with `TPFS_ALLOW_UNCONFINED=1` — re-checked for links right before.
	unanchored: Vec<PathBuf>,
	/// Private folders (trash) that could not be opened safely or are not private: never used.
	refused: Vec<PathBuf>,
}

impl Fs {
	/// Plain path-based operations (no roots): a test helper.
	#[allow(dead_code)]
	pub(crate) fn unconfined() -> Self {
		Self {
			#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
			anchors: Vec::new(),
			unanchored: Vec::new(),
			refused: Vec::new(),
		}
	}

	/// Like [`Fs::confined`], plus private folders outside the roots (per-file-system trash folders)
	/// that operations may also use: each is anchored the same way and must, on the opened handle,
	/// be owned by the user and closed to everyone else — else it gets no anchor and is refused.
	pub(crate) fn confined_with_private(roots: &[PathBuf], private: &[PathBuf]) -> Self {
		let mut fs = Self::confined(roots);
		#[cfg(any(target_os = "linux", target_vendor = "apple"))]
		for p in private {
			// A private folder that fails the check is refused whatever TPFS_ALLOW_UNCONFINED says.
			match confined::Anchor::open(p).filter(confined::Anchor::is_private) {
				Some(a) => fs.anchors.push(a),
				None => fs.refused.push(p.clone()),
			}
		}
		#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
		fs.refused.extend(private.iter().cloned());
		fs
	}

	/// Whether `dir` itself has a checked anchor (a root or a private folder).
	pub(crate) fn anchors_exactly(&self, dir: &Path) -> bool {
		#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
		{
			self.anchors.iter().any(|a| a.root() == dir)
		}
		#[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
		{
			let _ = dir;
			false
		}
	}

	/// Operations under any of `roots` are confined to it; roots are expected to be canonical.
	pub(crate) fn confined(roots: &[PathBuf]) -> Self {
		#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
		{
			let mut anchors = Vec::new();
			let mut unanchored = Vec::new();
			for r in roots {
				match confined::Anchor::open(r) {
					Some(a) => anchors.push(a),
					None => unanchored.push(r.clone()),
				}
			}
			Self { anchors, unanchored, refused: Vec::new() }
		}
		#[cfg(not(any(target_os = "linux", target_vendor = "apple", windows)))]
		{
			Self { unanchored: roots.to_vec(), refused: Vec::new() }
		}
	}

	/// Refuse a path whose closest root has no anchor (fail closed). With `TPFS_ALLOW_UNCONFINED=1`
	/// the path is instead re-checked for links right before the operation (a small window remains).
	fn guard(&self, path: &Path) -> io::Result<()> {
		if let Some(p) = self.refused.iter().find(|p| path.starts_with(p)) {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!(
					"refused: {} is not a private folder of yours (another owner, open to others, or a link)",
					crate::sanitize(p.as_os_str())
				),
			));
		}
		let deepest = |roots: &mut dyn Iterator<Item = &Path>| {
			roots.filter(|r| path.starts_with(r)).map(|r| r.as_os_str().len()).max()
		};
		let Some(bare) = deepest(&mut self.unanchored.iter().map(PathBuf::as_path)) else { return Ok(()) };
		#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
		if deepest(&mut self.anchors.iter().map(confined::Anchor::root)).is_some_and(|a| a > bare) {
			return Ok(()); // a nested root that is anchored
		}
		let _ = bare;
		if std::env::var_os("TPFS_ALLOW_UNCONFINED").is_none_or(|v| v != "1") {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!(
					"refused: the allowed folder holding {} cannot be opened safely (it is unreadable or a special \
					 reparse point), so operations in it cannot be confined; set TPFS_ALLOW_UNCONFINED=1 to proceed \
					 without that protection",
					crate::sanitize(path.as_os_str())
				),
			));
		}
		for root in self.unanchored.iter().filter(|r| path.starts_with(r)) {
			if let Ok(Some(link)) = crate::path::symlink_in_between(path, root) {
				return Err(io::Error::new(
					io::ErrorKind::PermissionDenied,
					format!("refused: {} goes through a link ({})", path.display(), link.display()),
				));
			}
		}
		Ok(())
	}

	/// Rename that fails with `AlreadyExists` instead of replacing the target, atomically where the
	/// platform can: `renameat2(RENAME_NOREPLACE)` on Linux, `renameatx_np(RENAME_EXCL)` on macOS,
	/// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` on Windows. Only a file system without such
	/// support (some network and FUSE file systems) falls back to check-then-rename.
	pub(crate) fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
		self.guard(from)?;
		self.guard(to)?;
		#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
		if let Some(result) = confined::rename(&self.anchors, from, to) {
			return result;
		}

		#[cfg(any(target_os = "linux", target_vendor = "apple"))]
		{
			use rustix::{
				fs::{CWD, RenameFlags, renameat_with},
				io::Errno,
			};
			match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
				Ok(()) => return Ok(()),
				Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => {}
				Err(e) => return Err(e.into()),
			}
		}

		#[cfg(windows)]
		{
			windows::move_noreplace(from, to)
		}
		#[cfg(not(windows))]
		{
			if fs::symlink_metadata(to).is_ok() {
				return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("target exists: {}", to.display())));
			}
			fs::rename(from, to)
		}
	}

	/// Remove a file or symlink (never a directory), confined like the other operations.
	pub(crate) fn remove_file(&self, path: &Path) -> io::Result<()> {
		self.guard(path)?;
		#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
		if let Some(result) = confined::unlink(&self.anchors, path) {
			return result;
		}
		#[cfg(windows)]
		if let Ok(m) = fs::symlink_metadata(path)
			&& m.permissions().readonly()
		{
			// Windows refuses to delete a read-only file; the copy already carries the attribute.
			let mut p = m.permissions();
			#[allow(clippy::permissions_set_readonly_false)]
			p.set_readonly(false);
			fs::set_permissions(path, p)?;
		}
		fs::remove_file(path)
	}

	/// Names in a directory, read through a confined descriptor where possible, so a directory
	/// swapped for a symlink is not followed out of the root.
	fn list(&self, dir: &Path) -> io::Result<Vec<std::ffi::OsString>> {
		Ok(self.list_entries(dir, false, usize::MAX)?.0.into_iter().map(|l| l.name).collect())
	}

	/// Entries of a directory with (when `meta`) their kind and size, all read relative to the
	/// directory's own confined handle, so nothing outside the roots is listed or measured.
	/// At most `max` entries are kept; the second value counts them all.
	pub(crate) fn list_entries(&self, dir: &Path, meta: bool, max: usize) -> io::Result<(Vec<Listed>, usize)> {
		self.guard(dir)?;
		#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
		if let Some(result) = confined::list(&self.anchors, dir, meta, max) {
			return result;
		}
		let mut out = Vec::new();
		let mut total = 0usize;
		for e in fs::read_dir(dir)? {
			let e = e?;
			total += 1;
			if out.len() >= max {
				continue;
			}
			let mut l = Listed { name: e.file_name(), ..Default::default() };
			if meta && let Ok(m) = fs::symlink_metadata(e.path()) {
				(l.dir, l.link, l.size) = (m.is_dir(), m.file_type().is_symlink(), m.len());
			}
			out.push(l);
		}
		Ok((out, total))
	}

	/// Permissions of a directory just created by a copy, set through a confined descriptor.
	fn set_dir_mode(&self, dir: &Path, perms: std::fs::Permissions) -> io::Result<()> {
		self.guard(dir)?;
		#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
		if let Some(result) = confined::set_dir_mode(&self.anchors, dir, perms.clone()) {
			return result;
		}
		set_dir_mode(dir, perms)
	}

	fn symlink(&self, target: &Path, path: &Path) -> io::Result<()> {
		self.guard(path)?;
		#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
		if let Some(result) = confined::symlink(&self.anchors, target, path) {
			return result;
		}
		make_symlink(target, path)
	}

	/// Move a file, directory or symlink. Within a file system this is one no-replace rename.
	///
	/// Across file systems the source is copied to `part` (a private name next to `to`), the copy is
	/// published at `to` with a no-replace rename, `publish` records that in the journal, and only then
	/// is the source removed — and only the entries that were copied and are unchanged since. Anything
	/// that appeared or changed in the source meanwhile is left where it is and reported as an error, so
	/// the step is rolled back with nothing lost.
	pub(crate) fn move_any(
		&self,
		from: &Path,
		to: &Path,
		part: &Path,
		publish: &mut dyn FnMut() -> io::Result<()>,
	) -> io::Result<()> {
		match self.rename_noreplace(from, to) {
			Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
				let copied = self.copy_published(from, to, part, publish)?;
				let (changed, stuck) = remove_copied(self, from, &copied)?;
				sync_parent(from);
				if let Some((path, e)) = stuck.first() {
					Err(io::Error::new(
						e.kind(),
						format!(
							"copied, but {} item(s) of the source could not be removed (e.g. {}: {e}); the move is undone",
							stuck.len(),
							path.display()
						),
					))
				} else if let Some(path) = changed.first() {
					Err(io::Error::other(format!(
						"{} item(s) changed or appeared in {} during the move and were left there, e.g. {}",
						changed.len(),
						from.display(),
						path.display()
					)))
				} else {
					Ok(())
				}
			}
			Ok(()) => {
				sync_parent(to);
				sync_parent(from);
				Ok(())
			}
			Err(e) => Err(e),
		}
	}

	/// Copy `from` to `to` through `part`: the copy only ever appears at `to` complete. `publish` runs
	/// right before the part is renamed into place (see `copy_published`).
	pub(crate) fn copy_to(
		&self,
		from: &Path,
		to: &Path,
		part: &Path,
		publish: &mut dyn FnMut() -> io::Result<()>,
	) -> io::Result<()> {
		self.copy_published(from, to, part, publish).map(|_| ())
	}

	fn copy_published(
		&self,
		from: &Path,
		to: &Path,
		part: &Path,
		publish: &mut dyn FnMut() -> io::Result<()>,
	) -> io::Result<Vec<Copied>> {
		let mut copied = Vec::new();
		if let Err(e) = self.copy_tree(from, part, Path::new(""), &mut copied) {
			// The partial copy is ours and the source is untouched: drop it.
			if let Err(r) = remove_part(self, part) {
				return Err(io::Error::new(
					e.kind(),
					format!("{e}; the partial copy {} could not be removed: {r}", part.display()),
				));
			}
			return Err(e);
		}
		sync_tree_dirs(part, &copied);
		// Journal "publishing" *before* the rename: afterwards the part name tells what happened —
		// still there, the rename did not happen; gone, the copy at `to` is ours. If the rename
		// fails, the part is left for the caller's clean-up, so that stays true even after a crash.
		if let Err(e) = publish() {
			// Nothing recorded: the part is still only ours.
			remove_part(self, part)?;
			return Err(e);
		}
		self.rename_noreplace(part, to)?;
		sync_parent(to);
		Ok(copied)
	}

	/// Copy a file, directory (recursively) or symlink (as a symlink), recording what was copied.
	/// Never overwrites. On Linux every entry under a root, at any depth, is listed, read and
	/// created through descriptors confined to that root.
	fn copy_tree(&self, from: &Path, to: &Path, rel: &Path, copied: &mut Vec<Copied>) -> io::Result<()> {
		let meta = fs::symlink_metadata(from)?;
		let ft = meta.file_type();

		if ft.is_file() {
			// Both sides first: one side under an anchored root must not let the other side, under a
			// root without an anchor, go through plain path calls unchecked.
			self.guard(from)?;
			self.guard(to)?;
			#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
			if let Some((src, dst)) = confined::open_copy_pair(&self.anchors, from, to, &meta)? {
				check_regular(&src, from)?;
				copy_stream(src, dst, from, &meta)?;
				copied.push(Copied::of(rel, &meta));
				return Ok(());
			}
			let src = open_regular(from)?;
			let dst = OpenOptions::new().write(true).create_new(true).open(to)?;
			copy_stream(src, dst, from, &meta)?;
		} else if ft.is_dir() {
			self.mkdir(to)?;
			copied.push(Copied::of(rel, &meta));
			// tpfs's own work entries (a volume's trash, partial copies) are never copied along.
			for name in self.list(from)?.into_iter().filter(|n| !crate::validate::is_own_name(n)) {
				// Never into another disk mounted below (like `cp -x`, but refusing: a move would
				// then empty that disk).
				#[cfg(unix)]
				{
					use std::os::unix::fs::MetadataExt;
					if fs::symlink_metadata(from.join(&name)).is_ok_and(|m| m.is_dir() && m.dev() != meta.dev()) {
						return Err(io::Error::new(
							io::ErrorKind::Unsupported,
							format!(
								"{} is a mount point (another disk inside the folder); it is not copied or moved",
								crate::sanitize(from.join(&name).as_os_str())
							),
						));
					}
				}
				self.copy_tree(&from.join(&name), &to.join(&name), &rel.join(&name), copied)?;
			}
			return self.set_dir_mode(to, safe_permissions(&meta));
		} else if ft.is_symlink() {
			self.symlink(&fs::read_link(from)?, to)?;
		} else {
			return Err(io::Error::new(io::ErrorKind::Unsupported, format!("special file: {}", from.display())));
		}
		copied.push(Copied::of(rel, &meta));
		Ok(())
	}

	pub(crate) fn mkdir(&self, path: &Path) -> io::Result<()> {
		self.guard(path)?;
		#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
		if let Some(result) = confined::mkdir(&self.anchors, path) {
			return result;
		}
		fs::create_dir(path)
	}

	pub(crate) fn rmdir(&self, path: &Path) -> io::Result<()> {
		self.guard(path)?;
		#[cfg(any(target_os = "linux", target_vendor = "apple", windows))]
		if let Some(result) = confined::rmdir(&self.anchors, path) {
			return result;
		}
		fs::remove_dir(path)
	}
}

fn copy_stream(mut src: File, mut dst: File, from: &Path, meta: &fs::Metadata) -> io::Result<()> {
	// std uses copy_file_range / sendfile / clonefile where available.
	let copied = io::copy(&mut src, &mut dst)?;
	if copied != meta.len() {
		return Err(io::Error::other(format!("short copy of {}: {copied} of {} bytes", from.display(), meta.len())));
	}
	// Keep the modification time, like `cp -p`: a copy is the same content, not a new version,
	// and folder comparison relies on it.
	if let Ok(t) = meta.modified() {
		dst.set_modified(t)?;
	}
	dst.set_permissions(safe_permissions(meta))?;
	dst.sync_all()
}

/// The source's permissions for its copy, without setuid and setgid: the copy belongs to whoever
/// runs tpfs, so keeping them would turn another user's program into one that runs as us
/// (`cp -p` drops them for the same reason when it cannot keep the owner).
fn safe_permissions(meta: &fs::Metadata) -> fs::Permissions {
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::Permissions::from_mode(meta.permissions().mode() & 0o1777)
	}
	#[cfg(not(unix))]
	meta.permissions()
}

#[cfg(unix)]
fn make_symlink(target: &Path, path: &Path) -> io::Result<()> {
	std::os::unix::fs::symlink(target, path)
}

#[cfg(not(unix))]
fn make_symlink(_: &Path, path: &Path) -> io::Result<()> {
	Err(io::Error::new(
		io::ErrorKind::Unsupported,
		format!("copying a symlink is not supported here: {}", path.display()),
	))
}

/// Open a file for copying, refusing anything that is not a regular file *now* (a FIFO swapped in
/// after validation would block forever; a device could be read endlessly). Never blocks on open.
fn open_regular(path: &Path) -> io::Result<File> {
	#[cfg(unix)]
	let file = {
		use rustix::fs::{Mode, OFlags};
		let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
		File::from(rustix::fs::open(path, flags, Mode::empty())?)
	};
	#[cfg(not(unix))]
	let file = File::open(path)?;
	check_regular(&file, path)?;
	Ok(file)
}

fn check_regular(file: &File, path: &Path) -> io::Result<()> {
	if file.metadata()?.is_file() {
		Ok(())
	} else {
		Err(io::Error::new(io::ErrorKind::Unsupported, format!("not a regular file (anymore): {}", path.display())))
	}
}

/// One entry copied by `copy_tree`, as the source looked when it was read.
pub(crate) struct Copied {
	rel: PathBuf,
	dir: bool,
	len: u64,
	modified: Option<std::time::SystemTime>,
}

impl Copied {
	fn of(rel: &Path, meta: &fs::Metadata) -> Self {
		Self { rel: rel.to_path_buf(), dir: meta.is_dir(), len: meta.len(), modified: meta.modified().ok() }
	}

	fn unchanged(&self, meta: &fs::Metadata) -> bool {
		meta.is_dir() == self.dir && (self.dir || (meta.len() == self.len && meta.modified().ok() == self.modified))
	}
}

/// Remove from `from` exactly the entries that were copied and have not changed since, children
/// before parents. Returns what was left in place: entries that changed (or a folder that got new
/// entries), and entries that could not be removed, with the reason (e.g. a read-only folder).
type Left = (Vec<PathBuf>, Vec<(PathBuf, io::Error)>);

fn remove_copied(fs: &Fs, from: &Path, copied: &[Copied]) -> io::Result<Left> {
	let mut left = Vec::new();
	let mut stuck = Vec::new();
	for c in copied.iter().rev() {
		let path = if c.rel.as_os_str().is_empty() { from.to_path_buf() } else { from.join(&c.rel) };
		let meta = match std::fs::symlink_metadata(&path) {
			Ok(m) => m,
			Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
			Err(e) => return Err(e),
		};
		if !c.unchanged(&meta) {
			left.push(path);
			continue;
		}
		match if c.dir { fs.rmdir(&path) } else { fs.remove_file(&path) } {
			Ok(()) => {}
			// A folder that still holds entries — ones we left, or new ones — changed meanwhile.
			Err(e) if c.dir && (e.kind() == io::ErrorKind::DirectoryNotEmpty || !left.is_empty()) => left.push(path),
			Err(e) => stuck.push((path, e)),
		}
	}
	Ok((left, stuck))
}

/// Make the directory entries of a freshly copied tree durable before it is published.
fn sync_tree_dirs(root: &Path, copied: &[Copied]) {
	for c in copied.iter().filter(|c| c.dir) {
		let dir = if c.rel.as_os_str().is_empty() { root.to_path_buf() } else { root.join(&c.rel) };
		sync_dir(&dir);
	}
}

/// Delete a partial copy that only tpfs ever saw (its private `part` name). Folders copied with a
/// read-only mode are made writable first (through `fs`, so a folder swapped for a symlink is not
/// followed), or the removal would fail and leave the copy behind.
pub(crate) fn remove_part(fs: &Fs, part: &Path) -> io::Result<()> {
	let m = match std::fs::symlink_metadata(part) {
		Ok(m) => m,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
		Err(e) => return Err(e),
	};
	if !m.is_dir() {
		return fs.remove_file(part);
	}
	make_writable(fs, part)?;
	// std's remove_dir_all does not follow symlinks inside the tree.
	std::fs::remove_dir_all(part)
}

/// Give every folder of our own tree owner write access (children before nothing else changes).
fn make_writable(fs: &Fs, dir: &Path) -> io::Result<()> {
	let m = std::fs::symlink_metadata(dir)?;
	if !m.is_dir() {
		return Ok(());
	}
	let mut p = m.permissions();
	#[cfg(unix)]
	std::os::unix::fs::PermissionsExt::set_mode(&mut p, 0o700);
	#[cfg(not(unix))]
	#[allow(clippy::permissions_set_readonly_false)]
	p.set_readonly(false);
	fs.set_dir_mode(dir, p)?;
	for name in fs.list(dir)? {
		make_writable(fs, &dir.join(name))?;
	}
	Ok(())
}

/// Set a folder's permissions through `fs` (confined on Linux and macOS).
#[cfg(unix)]
pub(crate) fn set_dir_mode_at(fs: &Fs, dir: &Path, perms: std::fs::Permissions) -> io::Result<()> {
	fs.set_dir_mode(dir, perms)
}

/// Set a directory's permissions through a handle opened without following a final symlink, so
/// a symlink swapped in for it cannot redirect the change to another file.
fn set_dir_mode(dir: &Path, perms: std::fs::Permissions) -> io::Result<()> {
	#[cfg(unix)]
	{
		use rustix::fs::{Mode, OFlags};
		let fd = rustix::fs::open(
			dir,
			OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
			Mode::empty(),
		)?;
		File::from(fd).set_permissions(perms)
	}
	#[cfg(not(unix))]
	{
		if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
			return Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused: became a link"));
		}
		std::fs::set_permissions(dir, perms)
	}
}

/// The private name a step's result is built under, next to its target.
pub(crate) fn part_path(to: &Path, tag: &str) -> PathBuf {
	to.with_file_name(format!(".tpfs-part-{tag}"))
}

/// Put back into `source` what a cross-device move had already removed from it, taking it from the
/// published copy at `target`; then move the rest of `target` (duplicates of what is still in
/// `source`) to `trash_rest`. Used when such a move stopped half way. Every step goes through `fs`
/// (confined to the roots): a folder swapped for a symlink meanwhile is refused, not followed.
pub(crate) fn merge_back(
	fs: &Fs,
	target: &Path,
	source: &Path,
	tag: &str,
	trash_rest: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
	let mut entries = Vec::new();
	collect(fs, target, Path::new(""), &mut entries)?;
	for (i, (rel, is_dir)) in entries.iter().enumerate() {
		let src = if rel.as_os_str().is_empty() { source.to_path_buf() } else { source.join(rel) };
		if *is_dir {
			if !crate::path::exists(&src) {
				fs.mkdir(&src)?;
			}
			continue;
		}
		if crate::path::exists(&src) {
			continue;
		}
		let part = part_path(&src, &format!("{tag}-restore-{i}"));
		remove_part(fs, &part)?; // a leftover of an earlier, interrupted attempt
		fs.move_any(&target.join(rel), &src, &part, &mut || Ok(()))?;
	}
	// Folders now empty were fully restored; whatever is left duplicates the source. It is our own
	// copy: make its folders writable, or a read-only one could not be moved into the trash.
	for (rel, _) in entries.iter().rev().filter(|(_, d)| *d) {
		let _ = fs.rmdir(&target.join(rel));
	}
	if crate::path::exists(target) {
		make_writable(fs, target)?;
		trash_rest(target)
	} else {
		Ok(())
	}
}

fn collect(fs: &Fs, path: &Path, rel: &Path, out: &mut Vec<(PathBuf, bool)>) -> io::Result<()> {
	let is_dir = std::fs::symlink_metadata(path)?.is_dir();
	out.push((rel.to_path_buf(), is_dir));
	if is_dir {
		for name in fs.list(path)? {
			collect(fs, &path.join(&name), &rel.join(&name), out)?;
		}
	}
	Ok(())
}

/// Make a directory entry change durable (best effort).
pub(crate) fn sync_parent(path: &Path) {
	if let Some(parent) = path.parent() {
		sync_dir(parent);
	}
}

/// fsync a directory so entries created in it survive a power loss (Unix; NTFS journals metadata).
fn sync_dir(dir: &Path) {
	// Not following a link, and never blocking (a FIFO swapped in for the folder must not hang a
	// batch that holds the journal lock).
	#[cfg(unix)]
	if let Ok(fd) = rustix::fs::open(
		dir,
		rustix::fs::OFlags::RDONLY
			| rustix::fs::OFlags::DIRECTORY
			| rustix::fs::OFlags::NOFOLLOW
			| rustix::fs::OFlags::NONBLOCK
			| rustix::fs::OFlags::CLOEXEC,
		rustix::fs::Mode::empty(),
	) {
		let _ = File::from(fd).sync_all();
	}
	#[cfg(not(unix))]
	let _ = dir;
}

/// Number of names (hard links) of the entry at `p`, not following a final link.
pub(crate) fn link_count(p: &Path) -> Option<u64> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt;
		fs::symlink_metadata(p).ok().map(|m| m.nlink())
	}
	#[cfg(windows)]
	{
		windows::link_count(p)
	}
	#[cfg(not(any(unix, windows)))]
	{
		let _ = p;
		None
	}
}

/// Whether two paths name the same file (same device and inode), e.g. `a.txt` and `A.txt`
/// on a case-insensitive file system.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt;
		match (fs::symlink_metadata(a), fs::symlink_metadata(b)) {
			(Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
			_ => false,
		}
	}
	#[cfg(windows)]
	{
		match (windows::file_id(a), windows::file_id(b)) {
			(Some(x), Some(y)) => x == y,
			_ => false,
		}
	}
	#[cfg(not(any(unix, windows)))]
	{
		let _ = (a, b);
		false
	}
}

#[cfg(any(target_os = "linux", target_vendor = "apple"))]
mod confined {
	use std::{
		ffi::OsStr,
		fs::{File, Metadata},
		io,
		os::{
			fd::{AsFd, BorrowedFd, OwnedFd},
			unix::fs::PermissionsExt,
		},
		path::{Path, PathBuf},
	};

	#[cfg(target_os = "linux")]
	use rustix::fs::{ResolveFlags, openat2};
	use rustix::{
		fs::{AtFlags, CWD, Mode, OFlags, RenameFlags, mkdirat, openat, renameat_with, unlinkat},
		io::Errno,
	};

	pub(super) struct Anchor {
		root: PathBuf,
		fd: OwnedFd,
	}

	impl Anchor {
		pub(super) fn root(&self) -> &Path {
			&self.root
		}

		/// Owned by the effective user and not open to group or others, judged on the open handle.
		#[allow(clippy::useless_conversion)] // st_mode is u16 on macOS
		pub(super) fn is_private(&self) -> bool {
			rustix::fs::fstat(&self.fd)
				.is_ok_and(|st| st.st_uid == rustix::process::geteuid().as_raw() && u32::from(st.st_mode) & 0o077 == 0)
		}

		/// The root is canonical, so it has no symlink in it: open it so that none is followed either,
		/// or a root swapped for a link after validation would bind every operation to the link's
		/// target (`openat2(RESOLVE_NO_SYMLINKS)` on Linux, `O_NOFOLLOW_ANY` on macOS).
		pub(super) fn open(root: &Path) -> Option<Self> {
			#[cfg(target_os = "linux")]
			let fd = match openat2(
				rustix::fs::CWD,
				root,
				DIR | OFlags::CLOEXEC,
				Mode::empty(),
				ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
			) {
				Ok(fd) => fd,
				// No openat2 (or no O_NOFOLLOW_ANY below): no anchor, so operations under this root
				// are refused, or with TPFS_ALLOW_UNCONFINED=1 re-checked for links right before.
				Err(_) => return None,
			};
			#[cfg(target_vendor = "apple")]
			let fd = {
				if !nofollow_any_works() {
					return None;
				}
				rustix::fs::open(root, DIR | OFlags::CLOEXEC | NOFOLLOW_ANY, Mode::empty()).ok()?
			};
			Some(Self { root: root.to_path_buf(), fd })
		}
	}

	/// Flags for a directory opened only to act relative to it.
	#[cfg(target_os = "linux")]
	const DIR: OFlags = OFlags::PATH.union(OFlags::DIRECTORY);
	#[cfg(target_vendor = "apple")]
	const DIR: OFlags = OFlags::RDONLY.union(OFlags::DIRECTORY);

	/// `O_NOFOLLOW_ANY` (macOS 11+): fail if any component of the path is a symlink.
	#[cfg(target_vendor = "apple")]
	const NOFOLLOW_ANY: OFlags = OFlags::from_bits_retain(0x2000_0000);

	/// Whether this macOS honours `O_NOFOLLOW_ANY` (11+). Older kernels silently ignore unknown flags,
	/// which would turn confinement off without a word: probe once with the system's own symlink
	/// `/tmp` (to `/private/tmp`), which must be refused. Not honoured = treated like a Linux kernel
	/// without openat2 (refuse, unless TPFS_ALLOW_UNCONFINED=1).
	#[cfg(target_vendor = "apple")]
	fn nofollow_any_works() -> bool {
		static WORKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
		*WORKS.get_or_init(|| {
			let probe = Path::new("/tmp");
			if !std::fs::symlink_metadata(probe).is_ok_and(|m| m.file_type().is_symlink()) {
				return true; // no probe available; the flag is known from macOS 11 on
			}
			rustix::fs::open(probe, OFlags::RDONLY | OFlags::DIRECTORY | NOFOLLOW_ANY, Mode::empty()).is_err()
		})
	}

	/// Open `rel` below the anchor without following any symlink and without leaving the root:
	/// `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` on Linux, `O_NOFOLLOW_ANY` on macOS (the path
	/// is already confined lexically; refusing every symlink keeps it there).
	fn open_beneath(anchor: &Anchor, rel: &Path, flags: OFlags) -> Result<OwnedFd, Errno> {
		#[cfg(target_os = "linux")]
		{
			openat2(&anchor.fd, rel, flags, Mode::empty(), RESOLVE)
		}
		#[cfg(target_vendor = "apple")]
		{
			if !nofollow_any_works() {
				return Err(Errno::NOSYS);
			}
			// Relative to the anchor's own descriptor, so the operation stays bound to the folder that
			// was opened and checked even if its path is taken over by another folder. Only plain
			// names may follow (no `..`), and no symlink anywhere (O_NOFOLLOW_ANY; it implies
			// O_NOFOLLOW, and macOS rejects the two together with EINVAL).
			if !rel.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir)) {
				return Err(Errno::XDEV);
			}
			rustix::fs::openat(&anchor.fd, rel, flags.difference(OFlags::NOFOLLOW) | NOFOLLOW_ANY, Mode::empty())
		}
	}

	/// Without `openat2` (Linux < 5.6, or a seccomp filter in a container) operations cannot be
	/// confined against symlinks swapped in after validation. tpfs refuses to run them then, unless
	/// the user accepts the weaker guarantee with `TPFS_ALLOW_UNCONFINED=1`.
	fn unconfined_ok() -> bool {
		std::env::var_os("TPFS_ALLOW_UNCONFINED").is_some_and(|v| v == "1")
	}

	fn no_openat2() -> io::Error {
		io::Error::new(
			io::ErrorKind::Unsupported,
			"this kernel has no openat2, so tpfs cannot confine operations to the allowed folders; \
			 set TPFS_ALLOW_UNCONFINED=1 to proceed without that protection",
		)
	}

	#[cfg(target_os = "linux")]
	const RESOLVE: ResolveFlags =
		ResolveFlags::BENEATH.union(ResolveFlags::NO_SYMLINKS).union(ResolveFlags::NO_MAGICLINKS);

	/// A path inside a root: its parent directory opened beneath the root, and its final name.
	/// `None` = not under any root, or `openat2` unsupported (kernel < 5.6): use path-based calls.
	fn locate<'p>(anchors: &[Anchor], path: &'p Path) -> Option<io::Result<(OwnedFd, &'p OsStr)>> {
		let anchor = anchors.iter().filter(|a| path.starts_with(&a.root)).max_by_key(|a| a.root.as_os_str().len())?;
		let rel = path.strip_prefix(&anchor.root).ok()?;
		let name = rel.file_name()?;
		let parent = rel.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));

		match open_beneath(anchor, parent, DIR | OFlags::CLOEXEC) {
			Ok(fd) => Some(Ok((fd, name))),
			Err(Errno::NOSYS) if unconfined_ok() => None,
			Err(Errno::NOSYS) => Some(Err(no_openat2())),
			Err(Errno::LOOP | Errno::XDEV) => Some(Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!("refused: {} goes through a symlink or leaves the allowed folder", path.display()),
			))),
			Err(e) => Some(Err(e.into())),
		}
	}

	/// Directory descriptor + name for one side of an operation: confined if located, else by path.
	fn side<'a>(located: &'a Option<(OwnedFd, &'a OsStr)>, path: &'a Path) -> (BorrowedFd<'a>, &'a OsStr) {
		match located {
			Some((fd, name)) => (fd.as_fd(), name),
			None => (CWD, path.as_os_str()),
		}
	}

	pub(super) fn rename(anchors: &[Anchor], from: &Path, to: &Path) -> Option<io::Result<()>> {
		let src = locate(anchors, from);
		let dst = locate(anchors, to);
		if src.is_none() && dst.is_none() {
			return None;
		}
		Some((|| {
			let src = src.transpose()?;
			let dst = dst.transpose()?;
			let (sfd, sname) = side(&src, from);
			let (dfd, dname) = side(&dst, to);
			match renameat_with(sfd, sname, dfd, dname, RenameFlags::NOREPLACE) {
				Ok(()) => Ok(()),
				// The file system does not support the flag: check, then rename.
				Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => {
					if rustix::fs::statat(dfd, dname, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
						return Err(io::Error::new(
							io::ErrorKind::AlreadyExists,
							format!("target exists: {}", to.display()),
						));
					}
					Ok(rustix::fs::renameat(sfd, sname, dfd, dname)?)
				}
				Err(e) => Err(e.into()),
			}
		})())
	}

	pub(super) fn set_dir_mode(anchors: &[Anchor], dir: &Path, perms: std::fs::Permissions) -> Option<io::Result<()>> {
		Some(locate(anchors, dir)?.and_then(|(fd, name)| {
			let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
			File::from(openat(&fd, name, flags, Mode::empty())?).set_permissions(perms)
		}))
	}

	pub(super) fn unlink(anchors: &[Anchor], path: &Path) -> Option<io::Result<()>> {
		Some(locate(anchors, path)?.and_then(|(fd, name)| Ok(unlinkat(&fd, name, AtFlags::empty())?)))
	}

	pub(super) fn symlink(anchors: &[Anchor], target: &Path, path: &Path) -> Option<io::Result<()>> {
		Some(locate(anchors, path)?.and_then(|(fd, name)| Ok(rustix::fs::symlinkat(target, &fd, name)?)))
	}

	/// List a directory opened beneath its root without following symlinks.
	pub(super) fn list(
		anchors: &[Anchor],
		dir: &Path,
		meta: bool,
		max: usize,
	) -> Option<io::Result<(Vec<super::Listed>, usize)>> {
		use std::os::unix::ffi::OsStrExt;
		let anchor = anchors.iter().filter(|a| dir.starts_with(&a.root)).max_by_key(|a| a.root.as_os_str().len())?;
		let rel = dir.strip_prefix(&anchor.root).ok()?;
		let rel = if rel.as_os_str().is_empty() { Path::new(".") } else { rel };
		let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
		let fd = match open_beneath(anchor, rel, flags) {
			Ok(fd) => fd,
			Err(Errno::NOSYS) if unconfined_ok() => return None,
			Err(Errno::NOSYS) => return Some(Err(no_openat2())),
			Err(Errno::LOOP | Errno::XDEV) => {
				return Some(Err(io::Error::new(
					io::ErrorKind::PermissionDenied,
					format!("refused: {} goes through a symlink or leaves the allowed folder", dir.display()),
				)));
			}
			Err(e) => return Some(Err(e.into())),
		};
		Some((|| {
			let mut names = Vec::new();
			let mut total = 0usize;
			for entry in rustix::fs::Dir::read_from(&fd)? {
				let entry = entry?;
				let name = entry.file_name().to_bytes();
				if name != b"." && name != b".." {
					total += 1;
					if names.len() >= max {
						continue;
					}
					let mut l = super::Listed { name: OsStr::from_bytes(name).to_os_string(), ..Default::default() };
					if meta && let Ok(st) = rustix::fs::statat(&fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW) {
						let ft = rustix::fs::FileType::from_raw_mode(st.st_mode as _);
						l.dir = ft == rustix::fs::FileType::Directory;
						l.link = ft == rustix::fs::FileType::Symlink;
						l.size = st.st_size as u64;
					}
					names.push(l);
				}
			}
			Ok((names, total))
		})())
	}

	pub(super) fn mkdir(anchors: &[Anchor], path: &Path) -> Option<io::Result<()>> {
		Some(locate(anchors, path)?.and_then(|(fd, name)| Ok(mkdirat(&fd, name, Mode::from_raw_mode(0o777))?)))
	}

	pub(super) fn rmdir(anchors: &[Anchor], path: &Path) -> Option<io::Result<()>> {
		Some(locate(anchors, path)?.and_then(|(fd, name)| Ok(unlinkat(&fd, name, AtFlags::REMOVEDIR)?)))
	}

	/// Open a file to copy and create its copy, both through confined descriptors where possible.
	/// `Ok(None)` when neither side is under a root.
	pub(super) fn open_copy_pair(
		anchors: &[Anchor],
		from: &Path,
		to: &Path,
		meta: &Metadata,
	) -> io::Result<Option<(File, File)>> {
		let src = locate(anchors, from).transpose()?;
		let dst = locate(anchors, to).transpose()?;
		if src.is_none() && dst.is_none() {
			return Ok(None);
		}

		let (sfd, sname) = side(&src, from);
		let src =
			openat(sfd, sname, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC, Mode::empty())?;

		let (dfd, dname) = side(&dst, to);
		let create = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
		let dst = openat(
			dfd,
			dname,
			create,
			Mode::from_raw_mode((meta.permissions().mode() & 0o1777) as rustix::fs::RawMode),
		)?;

		Ok(Some((File::from(src), File::from(dst))))
	}
}

/// Windows: operations inside the roots run relative to directory handles. Every folder on the way
/// from the root is opened by itself with `NtCreateFile` (relative to the previous handle,
/// `FILE_OPEN_REPARSE_POINT`), refused if it is a reparse point (symlink, junction, mount point), and
/// kept open without `FILE_SHARE_DELETE` until the operation is done — so nobody can rename it away
/// or swap a link in for it meanwhile. The operation itself goes through a handle too (rename by
/// `FILE_RENAME_INFO` with a root directory, delete by disposition, create relative to the parent).
#[cfg(windows)]
mod confined {
	use std::{
		ffi::{OsStr, OsString},
		fs::{File, Metadata},
		io,
		os::windows::{
			ffi::{OsStrExt, OsStringExt},
			io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
		},
		path::{Component, Path, PathBuf},
	};

	use windows_sys::{
		Wdk::{
			Foundation::OBJECT_ATTRIBUTES,
			Storage::FileSystem::{
				FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
				FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation, NtCreateFile,
				NtSetInformationFile,
			},
		},
		Win32::{
			Foundation::{HANDLE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING},
			Storage::FileSystem::{
				CreateFileW, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY,
				FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_DISPOSITION_INFO,
				FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FULL_DIR_INFO, FILE_GENERIC_READ,
				FILE_GENERIC_WRITE, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
				FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FileAttributeTagInfo,
				FileBasicInfo, FileDispositionInfo, FileFullDirectoryInfo, FileFullDirectoryRestartInfo, FileIdInfo,
				GetFileInformationByHandleEx, OPEN_EXISTING, SYNCHRONIZE, SetFileInformationByHandle,
			},
			System::IO::IO_STATUS_BLOCK,
		},
	};

	pub(super) struct Anchor {
		root: PathBuf,
		handle: OwnedHandle,
	}

	impl Anchor {
		pub(super) fn root(&self) -> &Path {
			&self.root
		}

		/// Opened component by component from the drive or share root, refusing any link on the way
		/// (not only at the end): a root swapped for a junction after validation is not followed.
		pub(super) fn open(root: &Path) -> Option<Self> {
			use std::path::Component;
			let mut comps = root.components();
			let mut base = PathBuf::new();
			for c in comps.by_ref() {
				base.push(c.as_os_str());
				if matches!(c, Component::RootDir) {
					break;
				}
			}
			let mut handle = Self::open_base(&base)?;
			for c in comps {
				let Component::Normal(name) = c else { return None };
				let h = nt_open(
					raw(&handle),
					name,
					FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES,
					FILE_SHARE_READ | FILE_SHARE_WRITE,
					FILE_OPEN,
					FILE_DIRECTORY_FILE,
				)
				.ok()?;
				if is_reparse(&h).ok()? {
					return None;
				}
				handle = h;
			}
			Some(Self { root: root.to_path_buf(), handle })
		}

		fn open_base(root: &Path) -> Option<OwnedHandle> {
			let w: Vec<u16> = crate::path::win_api_path(root).encode_wide().chain(Some(0)).collect();
			// SAFETY: `w` is NUL-terminated and outlives the call; no security attributes or template.
			let h = unsafe {
				CreateFileW(
					w.as_ptr(),
					FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
					FILE_SHARE_READ | FILE_SHARE_WRITE,
					std::ptr::null(),
					OPEN_EXISTING,
					FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
					std::ptr::null_mut(),
				)
			};
			if h == INVALID_HANDLE_VALUE {
				return None;
			}
			// SAFETY: `h` is a fresh, valid handle that nothing else owns.
			let handle = unsafe { OwnedHandle::from_raw_handle(h as RawHandle) };
			(!is_reparse(&handle).ok()?).then_some(handle)
		}
	}

	fn raw(h: &OwnedHandle) -> HANDLE {
		h.as_raw_handle() as HANDLE
	}

	fn refused(path: &Path) -> io::Error {
		io::Error::new(
			io::ErrorKind::PermissionDenied,
			format!("refused: {} goes through a link (symlink, junction or mount point)", path.display()),
		)
	}

	/// Whether `h` is a reparse point of any kind (a link, or a file whose data a filter driver
	/// supplies: compressed by WOF, deduplicated, a cloud placeholder).
	fn any_reparse(h: &OwnedHandle) -> io::Result<bool> {
		let mut info = FILE_ATTRIBUTE_TAG_INFO { FileAttributes: 0, ReparseTag: 0 };
		// SAFETY: `info` is a valid buffer of the size passed for this information class.
		let ok = unsafe {
			GetFileInformationByHandleEx(
				raw(h),
				FileAttributeTagInfo,
				(&raw mut info).cast(),
				size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
			)
		};
		if ok == 0 {
			return Err(io::Error::last_os_error());
		}
		Ok(info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0)
	}

	/// The volume and 128-bit file id behind a handle.
	fn id_of(h: &OwnedHandle) -> io::Result<(u64, [u8; 16])> {
		// SAFETY: an all-zero FILE_ID_INFO is a valid value; the buffer has the size passed.
		let mut id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
		// SAFETY: `h` is open and `id` is a valid out-buffer of the size passed.
		let ok = unsafe {
			GetFileInformationByHandleEx(raw(h), FileIdInfo, (&raw mut id).cast(), size_of::<FILE_ID_INFO>() as u32)
		};
		if ok == 0 { Err(io::Error::last_os_error()) } else { Ok((id.VolumeSerialNumber, id.FileId.Identifier)) }
	}

	/// Whether `h` is a link: a reparse point whose tag is a name surrogate.
	fn is_reparse(h: &OwnedHandle) -> io::Result<bool> {
		let mut info = FILE_ATTRIBUTE_TAG_INFO { FileAttributes: 0, ReparseTag: 0 };
		// SAFETY: `info` is a valid buffer of the size passed for this information class.
		let ok = unsafe {
			GetFileInformationByHandleEx(
				raw(h),
				FileAttributeTagInfo,
				(&raw mut info).cast(),
				size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
			)
		};
		if ok == 0 {
			return Err(io::Error::last_os_error());
		}
		// Only name surrogates (symlinks, junctions, mount points…) redirect a path; other reparse points
		// (OneDrive / Cloud Files placeholders, deduplicated files, app links) are ordinary entries.
		const NAME_SURROGATE: u32 = 0x2000_0000;
		Ok(info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && info.ReparseTag & NAME_SURROGATE != 0)
	}

	/// `NtCreateFile` of `name` relative to the directory `root`, never following a reparse point.
	fn nt_open(
		root: HANDLE,
		name: &OsStr,
		access: u32,
		share: u32,
		disposition: u32,
		options: u32,
	) -> io::Result<OwnedHandle> {
		nt_open_with(root, name, access, share, disposition, options | FILE_OPEN_REPARSE_POINT)
	}

	/// `NtCreateFile` relative to `root` with exactly these options (callers add
	/// `FILE_OPEN_REPARSE_POINT` unless they deliberately let a filter driver supply the data).
	fn nt_open_with(
		root: HANDLE,
		name: &OsStr,
		access: u32,
		share: u32,
		disposition: u32,
		options: u32,
	) -> io::Result<OwnedHandle> {
		let mut wide: Vec<u16> = name.encode_wide().collect();
		let bytes = u16::try_from(wide.len() * 2).map_err(|_| io::Error::other("name too long"))?;
		let us = UNICODE_STRING { Length: bytes, MaximumLength: bytes, Buffer: wide.as_mut_ptr() };
		let attrs = OBJECT_ATTRIBUTES {
			Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
			RootDirectory: root,
			ObjectName: &us,
			Attributes: OBJ_CASE_INSENSITIVE,
			SecurityDescriptor: std::ptr::null(),
			SecurityQualityOfService: std::ptr::null(),
		};
		// SAFETY: an all-zero IO_STATUS_BLOCK is a valid initial value.
		let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
		let mut h: HANDLE = std::ptr::null_mut();
		// SAFETY: every pointer is valid for the call (`us` and `wide` outlive it); no EA buffer.
		let status = unsafe {
			NtCreateFile(
				&mut h,
				access | SYNCHRONIZE,
				&attrs,
				&mut iosb,
				std::ptr::null(),
				FILE_ATTRIBUTE_NORMAL,
				share,
				disposition,
				options | FILE_SYNCHRONOUS_IO_NONALERT,
				std::ptr::null(),
				0,
			)
		};
		if status < 0 {
			// SAFETY: plain conversion of a status code.
			let code = unsafe { RtlNtStatusToDosError(status) };
			return Err(io::Error::from_raw_os_error(code as i32));
		}
		// SAFETY: NtCreateFile succeeded, so `h` is a fresh handle that nothing else owns.
		Ok(unsafe { OwnedHandle::from_raw_handle(h as RawHandle) })
	}

	/// The open folder chain from the root to `path`'s parent (held, so no folder on the way can be
	/// renamed or replaced), and the final name. `None` when `path` is under no root.
	struct Located<'p> {
		chain: Vec<OwnedHandle>,
		name: &'p OsStr,
	}

	impl Located<'_> {
		fn parent(&self, anchor: &Anchor) -> HANDLE {
			self.chain.last().map_or(raw(&anchor.handle), raw)
		}
	}

	fn anchor_of<'a>(anchors: &'a [Anchor], path: &Path) -> Option<&'a Anchor> {
		anchors.iter().filter(|a| path.starts_with(&a.root)).max_by_key(|a| a.root.as_os_str().len())
	}

	fn locate<'p>(anchor: &Anchor, path: &'p Path) -> io::Result<Located<'p>> {
		let rel = path.strip_prefix(&anchor.root).map_err(|_| refused(path))?;
		let name = rel.file_name().ok_or_else(|| refused(path))?;
		let mut chain: Vec<OwnedHandle> = Vec::new();
		for c in rel.parent().into_iter().flat_map(Path::components) {
			let Component::Normal(part) = c else { return Err(refused(path)) };
			let parent = chain.last().map_or(raw(&anchor.handle), raw);
			let h = nt_open(
				parent,
				part,
				FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES,
				FILE_SHARE_READ | FILE_SHARE_WRITE,
				FILE_OPEN,
				FILE_DIRECTORY_FILE,
			)?;
			if is_reparse(&h)? {
				return Err(refused(path));
			}
			chain.push(h);
		}
		Ok(Located { chain, name })
	}

	/// Open the final component itself (not following a link), for renaming or deleting it.
	fn open_final(parent: HANDLE, name: &OsStr, access: u32, options: u32, path: &Path) -> io::Result<OwnedHandle> {
		let h =
			nt_open(parent, name, access, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, FILE_OPEN, options)?;
		let _ = path;
		Ok(h)
	}

	/// `\??\`-prefixed NT path for a rename target that is not under a root (the trash).
	fn nt_path(p: &Path) -> Vec<u16> {
		let s = p.as_os_str().to_string_lossy();
		let nt = match s.strip_prefix(r"\\") {
			Some(unc) => format!(r"\??\UNC\{unc}"),
			None => format!(r"\??\{s}"),
		};
		OsStr::new(&nt).encode_wide().collect()
	}

	/// Rename the object behind `h` to `name` under the directory `root` (or to the full NT path
	/// `name` when `root` is null), never replacing anything. Calls `NtSetInformationFile` directly:
	/// the Win32 wrapper does not take a relative name with a root handle.
	fn rename_by_handle(h: &OwnedHandle, root: HANDLE, name: &[u16]) -> io::Result<()> {
		let header = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
		let size = (header + name.len() * 2).max(std::mem::size_of::<FILE_RENAME_INFORMATION>());
		let mut buf = vec![0u64; size.div_ceil(8)];
		let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
		// SAFETY: `buf` is zeroed, large enough and suitably aligned for the header plus the name;
		// the fields are written through raw pointers into that buffer only.
		unsafe {
			(*info).RootDirectory = root;
			(*info).FileNameLength = (name.len() * 2) as u32;
			std::ptr::copy_nonoverlapping(name.as_ptr(), (&raw mut (*info).FileName).cast::<u16>(), name.len());
		}
		// SAFETY: IO_STATUS_BLOCK is plain data; all zeroes is a valid value.
		let mut iosb: IO_STATUS_BLOCK = unsafe { std::mem::zeroed() };
		// SAFETY: `info` points at a complete FILE_RENAME_INFORMATION of `size` bytes; `iosb` is ours.
		let status =
			unsafe { NtSetInformationFile(raw(h), &mut iosb, info.cast(), size as u32, FileRenameInformation) };
		if status < 0 {
			// SAFETY: plain conversion of a status code.
			let code = unsafe { RtlNtStatusToDosError(status) };
			return Err(io::Error::from_raw_os_error(code as i32));
		}
		Ok(())
	}

	pub(super) fn rename(anchors: &[Anchor], from: &Path, to: &Path) -> Option<io::Result<()>> {
		let (sa, da) = (anchor_of(anchors, from), anchor_of(anchors, to));
		if sa.is_none() && da.is_none() {
			return None;
		}
		Some((|| {
			let src_loc = sa.map(|a| locate(a, from)).transpose()?;
			let dst_loc = da.map(|a| locate(a, to)).transpose()?;
			let access = DELETE | FILE_READ_ATTRIBUTES;
			let h = match (&src_loc, sa) {
				(Some(l), Some(a)) => open_final(l.parent(a), l.name, access, 0, from)?,
				_ => open_path(from, access)?,
			};
			match (&dst_loc, da) {
				(Some(l), Some(a)) => {
					let name: Vec<u16> = l.name.encode_wide().collect();
					rename_by_handle(&h, l.parent(a), &name)
				}
				_ => rename_by_handle(&h, std::ptr::null_mut(), &nt_path(to)),
			}
		})())
	}

	/// Open a path outside the roots (the private trash) itself, not following a final link.
	fn open_path(p: &Path, access: u32) -> io::Result<OwnedHandle> {
		let w: Vec<u16> = crate::path::win_api_path(p).encode_wide().chain(Some(0)).collect();
		// SAFETY: `w` is NUL-terminated and outlives the call.
		let h = unsafe {
			CreateFileW(
				w.as_ptr(),
				access,
				FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
				std::ptr::null(),
				OPEN_EXISTING,
				FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
				std::ptr::null_mut(),
			)
		};
		if h == INVALID_HANDLE_VALUE {
			return Err(io::Error::last_os_error());
		}
		// SAFETY: a fresh handle owned by nothing else.
		Ok(unsafe { OwnedHandle::from_raw_handle(h as RawHandle) })
	}

	fn with_located<T>(
		anchors: &[Anchor],
		path: &Path,
		f: impl FnOnce(HANDLE, &OsStr) -> io::Result<T>,
	) -> Option<io::Result<T>> {
		let a = anchor_of(anchors, path)?;
		Some(locate(a, path).and_then(|l| f(l.parent(a), l.name)))
	}

	pub(super) fn mkdir(anchors: &[Anchor], path: &Path) -> Option<io::Result<()>> {
		with_located(anchors, path, |parent, name| {
			nt_open(
				parent,
				name,
				FILE_LIST_DIRECTORY,
				FILE_SHARE_READ | FILE_SHARE_WRITE,
				FILE_CREATE,
				FILE_DIRECTORY_FILE,
			)
			.map(drop)
		})
	}

	fn delete(h: &OwnedHandle) -> io::Result<()> {
		let info = FILE_DISPOSITION_INFO { DeleteFile: true };
		// SAFETY: `info` is a valid FILE_DISPOSITION_INFO of the size passed.
		let ok = unsafe {
			SetFileInformationByHandle(
				raw(h),
				FileDispositionInfo,
				(&raw const info).cast(),
				size_of::<FILE_DISPOSITION_INFO>() as u32,
			)
		};
		if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
	}

	fn basic(h: &OwnedHandle) -> io::Result<FILE_BASIC_INFO> {
		// SAFETY: an all-zero FILE_BASIC_INFO is a valid value.
		let mut info: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
		// SAFETY: `info` is a valid buffer of the size passed.
		let ok = unsafe {
			GetFileInformationByHandleEx(
				raw(h),
				FileBasicInfo,
				(&raw mut info).cast(),
				size_of::<FILE_BASIC_INFO>() as u32,
			)
		};
		if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(info) }
	}

	fn set_basic(h: &OwnedHandle, info: &FILE_BASIC_INFO) -> io::Result<()> {
		// SAFETY: `info` is a valid FILE_BASIC_INFO of the size passed.
		let ok = unsafe {
			SetFileInformationByHandle(
				raw(h),
				FileBasicInfo,
				(info as *const FILE_BASIC_INFO).cast(),
				size_of::<FILE_BASIC_INFO>() as u32,
			)
		};
		if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
	}

	pub(super) fn rmdir(anchors: &[Anchor], path: &Path) -> Option<io::Result<()>> {
		with_located(anchors, path, |parent, name| {
			let h = open_final(
				parent,
				name,
				DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
				FILE_DIRECTORY_FILE,
				path,
			)?;
			delete_writable(&h)
		})
	}

	/// Delete through `h`, clearing a read-only attribute first (NTFS refuses to delete read-only
	/// files and folders; Explorer marks customised folders read-only). If the delete still fails,
	/// the attribute is put back.
	fn delete_writable(h: &OwnedHandle) -> io::Result<()> {
		let original = basic(h)?;
		let read_only = original.FileAttributes & FILE_ATTRIBUTE_READONLY != 0;
		if read_only {
			let mut b = basic(h)?;
			b.FileAttributes &= !FILE_ATTRIBUTE_READONLY;
			set_basic(h, &b)?;
		}
		let result = delete(h);
		if result.is_err() && read_only {
			let _ = set_basic(h, &original);
		}
		result
	}

	pub(super) fn unlink(anchors: &[Anchor], path: &Path) -> Option<io::Result<()>> {
		with_located(anchors, path, |parent, name| {
			let h = open_final(
				parent,
				name,
				DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
				FILE_NON_DIRECTORY_FILE,
				path,
			)?;
			delete_writable(&h)
		})
	}

	pub(super) fn set_dir_mode(anchors: &[Anchor], dir: &Path, perms: std::fs::Permissions) -> Option<io::Result<()>> {
		with_located(anchors, dir, |parent, name| {
			let h = open_final(parent, name, FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES, FILE_DIRECTORY_FILE, dir)?;
			if is_reparse(&h)? {
				return Err(refused(dir));
			}
			let mut b = basic(&h)?;
			if perms.readonly() {
				b.FileAttributes |= FILE_ATTRIBUTE_READONLY;
			} else {
				b.FileAttributes &= !FILE_ATTRIBUTE_READONLY;
			}
			set_basic(&h, &b)
		})
	}

	pub(super) fn symlink(anchors: &[Anchor], _target: &Path, path: &Path) -> Option<io::Result<()>> {
		anchor_of(anchors, path)?;
		Some(Err(io::Error::new(
			io::ErrorKind::Unsupported,
			format!("copying a symlink is not supported here: {}", path.display()),
		)))
	}

	pub(super) fn list(
		anchors: &[Anchor],
		dir: &Path,
		_meta: bool,
		max: usize,
	) -> Option<io::Result<(Vec<super::Listed>, usize)>> {
		let a = anchor_of(anchors, dir)?;
		Some((|| {
			let h = if dir == a.root {
				None
			} else {
				let l = locate(a, dir)?;
				let h = nt_open(
					l.parent(a),
					l.name,
					FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
					FILE_SHARE_READ | FILE_SHARE_WRITE,
					FILE_OPEN,
					FILE_DIRECTORY_FILE,
				)?;
				if is_reparse(&h)? {
					return Err(refused(dir));
				}
				Some(h)
			};
			let dh = h.as_ref().map_or(raw(&a.handle), raw);
			let mut names = Vec::new();
			let mut total = 0usize;
			let mut buf = vec![0u64; 8192];
			let mut class = FileFullDirectoryRestartInfo;
			loop {
				// SAFETY: `buf` is a valid, 8-byte aligned buffer of the size passed.
				let ok =
					unsafe { GetFileInformationByHandleEx(dh, class, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32) };
				if ok == 0 {
					let e = io::Error::last_os_error();
					// ERROR_NO_MORE_FILES
					if e.raw_os_error() == Some(18) {
						break;
					}
					return Err(e);
				}
				class = FileFullDirectoryInfo;
				let base = buf.as_ptr().cast::<u8>();
				let mut offset = 0usize;
				loop {
					// SAFETY: the system filled `buf` with FILE_FULL_DIR_INFO records chained by
					// NextEntryOffset; `offset` always points at the start of one of them.
					let (next, name, attrs, tag, size) = unsafe {
						let e = base.add(offset).cast::<FILE_FULL_DIR_INFO>();
						let len = (*e).FileNameLength as usize / 2;
						let p = (&raw const (*e).FileName).cast::<u16>();
						(
							(*e).NextEntryOffset as usize,
							std::slice::from_raw_parts(p, len).to_vec(),
							(*e).FileAttributes,
							(*e).EaSize, // holds the reparse tag for reparse points
							(*e).EndOfFile,
						)
					};
					let name = OsString::from_wide(&name);
					if name != "." && name != ".." && {
						total += 1;
						names.len() < max
					} {
						let link = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag & 0x2000_0000 != 0;
						names.push(super::Listed {
							name,
							dir: !link && attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
							link,
							size: size.max(0) as u64,
						});
					}
					if next == 0 {
						break;
					}
					offset += next;
				}
			}
			Ok((names, total))
		})())
	}

	/// Open a file to copy and create its copy, both relative to held folder handles where possible.
	pub(super) fn open_copy_pair(
		anchors: &[Anchor],
		from: &Path,
		to: &Path,
		meta: &Metadata,
	) -> io::Result<Option<(File, File)>> {
		let (sa, da) = (anchor_of(anchors, from), anchor_of(anchors, to));
		if sa.is_none() && da.is_none() {
			return Ok(None);
		}
		let _ = meta;
		let src = match sa {
			Some(a) => {
				let l = locate(a, from)?;
				let h = nt_open(
					l.parent(a),
					l.name,
					FILE_GENERIC_READ,
					FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
					FILE_OPEN,
					FILE_NON_DIRECTORY_FILE,
				)?;
				if is_reparse(&h)? {
					return Err(refused(from));
				}
				if any_reparse(&h)? {
					// Not a link, but its data comes from a filter driver (WOF compression, dedup, a
					// cloud placeholder): read through the driver, or the copy gets the raw on-disk
					// stream (zeros). Reopened relative to the same held folder; it must be the same file.
					let data = nt_open_with(
						l.parent(a),
						l.name,
						FILE_GENERIC_READ,
						FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
						FILE_OPEN,
						FILE_NON_DIRECTORY_FILE,
					)?;
					if id_of(&data)? != id_of(&h)? {
						return Err(refused(from));
					}
					File::from(data)
				} else {
					File::from(h)
				}
			}
			None => File::open(from)?,
		};
		let dst = match da {
			Some(a) => {
				let l = locate(a, to)?;
				File::from(nt_open(
					l.parent(a),
					l.name,
					FILE_GENERIC_WRITE | FILE_READ_ATTRIBUTES,
					0,
					FILE_CREATE,
					FILE_NON_DIRECTORY_FILE,
				)?)
			}
			None => std::fs::OpenOptions::new().write(true).create_new(true).open(to)?,
		};
		Ok(Some((src, dst)))
	}
}

#[cfg(windows)]
mod windows {
	use std::{io, os::windows::ffi::OsStrExt, path::Path};

	use windows_sys::Win32::{
		Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
		Storage::FileSystem::{
			BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
			FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandle,
			GetFileInformationByHandleEx, MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING,
		},
	};

	/// (volume serial, file id): the identity of a file, whatever name reaches it.
	pub(super) fn file_id(p: &Path) -> Option<(u64, u128)> {
		let w = wide(p);
		// SAFETY: `w` is NUL-terminated and outlives the call; no security attributes or template.
		let h = unsafe {
			CreateFileW(
				w.as_ptr(),
				0,
				FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
				std::ptr::null(),
				OPEN_EXISTING,
				FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
				std::ptr::null_mut(),
			)
		};
		if h == INVALID_HANDLE_VALUE {
			return None;
		}
		// 128-bit id first (ReFS does not guarantee unique 64-bit indexes).
		// SAFETY: an all-zero FILE_ID_INFO is a valid value; the buffer has the size passed.
		let mut id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
		let ok = unsafe {
			GetFileInformationByHandleEx(h, FileIdInfo, (&raw mut id).cast(), size_of::<FILE_ID_INFO>() as u32)
		};
		if ok != 0 {
			// SAFETY: `h` was returned by CreateFileW above and is not used after this.
			unsafe { CloseHandle(h) };
			return Some((id.VolumeSerialNumber, u128::from_le_bytes(id.FileId.Identifier)));
		}
		// SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value of this plain C struct.
		let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
		// SAFETY: `h` is an open handle and `info` a valid out-pointer; the handle is closed once.
		let ok = unsafe { GetFileInformationByHandle(h, &mut info) };
		// SAFETY: `h` was returned by CreateFileW above and is not used after this.
		unsafe { CloseHandle(h) };
		(ok != 0).then(|| {
			(
				u64::from(info.dwVolumeSerialNumber),
				u128::from((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)),
			)
		})
	}

	pub(super) fn link_count(p: &Path) -> Option<u64> {
		let w = wide(p);
		// SAFETY: `w` is NUL-terminated and outlives the call; no security attributes or template.
		let h = unsafe {
			CreateFileW(
				w.as_ptr(),
				0,
				FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
				std::ptr::null(),
				OPEN_EXISTING,
				FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
				std::ptr::null_mut(),
			)
		};
		if h == INVALID_HANDLE_VALUE {
			return None;
		}
		// SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value of this plain C struct.
		let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
		// SAFETY: `h` is an open handle and `info` a valid out-pointer.
		let ok = unsafe { GetFileInformationByHandle(h, &mut info) };
		// SAFETY: `h` was returned by CreateFileW above and is not used after this.
		unsafe { CloseHandle(h) };
		(ok != 0).then_some(u64::from(info.nNumberOfLinks))
	}

	fn wide(p: &Path) -> Vec<u16> {
		crate::path::win_api_path(p).encode_wide().chain(Some(0)).collect()
	}

	/// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` (fails with ERROR_ALREADY_EXISTS) and without
	/// `MOVEFILE_COPY_ALLOWED` (another volume fails with ERROR_NOT_SAME_DEVICE = CrossesDevices).
	pub(super) fn move_noreplace(from: &Path, to: &Path) -> io::Result<()> {
		let (from, to) = (wide(from), wide(to));
		// SAFETY: both buffers are NUL-terminated UTF-16 strings that outlive the call.
		let ok = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
		if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
	}
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
	use std::os::unix::fs::MetadataExt;

	use super::*;

	/// Two directories on different file systems, or `None` when the machine has only one.
	fn two_devices() -> Option<(tempfile::TempDir, tempfile::TempDir)> {
		let a = tempfile::TempDir::new_in("/dev/shm").ok()?;
		let b = tempfile::TempDir::new().ok()?;
		let dev = |p: &Path| fs::metadata(p).map(|m| m.dev()).ok();
		(dev(a.path()) != dev(b.path())).then_some((a, b))
	}

	#[test]
	fn cross_device_move_keeps_what_appeared_in_the_source() {
		let Some((a, b)) = two_devices() else {
			assert!(std::env::var_os("CI").is_none(), "CI must provide /dev/shm on its own file system");
			return;
		};
		let from = a.path().join("inbox");
		fs::create_dir(&from).unwrap();
		fs::write(from.join("photo.jpg"), "photo").unwrap();
		let to = b.path().join("inbox");

		let late = from.join("late.txt");
		let err = Fs::unconfined()
			.move_any(&from, &to, &part_path(&to, "t"), &mut || fs::write(&late, "written meanwhile"))
			.unwrap_err();
		let fs_ = Fs::unconfined();
		assert!(err.to_string().contains("left there"), "{err}");
		assert_eq!(fs::read_to_string(&late).unwrap(), "written meanwhile");
		assert_eq!(fs::read_to_string(to.join("photo.jpg")).unwrap(), "photo");
		assert!(!from.join("photo.jpg").exists());

		// Rolling back puts the moved file home and keeps the late one.
		let mut rest = Vec::new();
		merge_back(&fs_, &to, &from, "t", &mut |p| {
			rest.push(p.to_path_buf());
			Ok(())
		})
		.unwrap();
		assert_eq!(fs::read_to_string(from.join("photo.jpg")).unwrap(), "photo");
		assert_eq!(fs::read_to_string(&late).unwrap(), "written meanwhile");
		assert!(rest.is_empty() && !to.exists(), "{rest:?}");
	}

	#[test]
	fn cross_device_copy_never_shows_a_partial_target() {
		let Some((a, b)) = two_devices() else {
			assert!(std::env::var_os("CI").is_none(), "CI must provide /dev/shm on its own file system");
			return;
		};
		let from = a.path().join("dir");
		fs::create_dir(&from).unwrap();
		fs::write(from.join("x"), "x").unwrap();
		fs::create_dir(from.join("sub")).unwrap();
		let to = b.path().join("dir");
		let part = part_path(&to, "t");

		// Publishing fails (as if the journal could not be written): no target, no part.
		let err = Fs::unconfined().copy_to(&from, &to, &part, &mut || {
			// Recorded right before the rename: the complete copy is still under its part name.
			assert!(part.join("sub").is_dir() && !to.exists());
			Err(io::Error::other("journal full"))
		});
		assert!(err.is_err());
		assert!(!part.exists());
		assert!(from.join("x").exists());
	}

	#[test]
	fn merge_back_restores_a_half_removed_tree() {
		let t = tempfile::TempDir::new().unwrap();
		let (target, source) = (t.path().join("target"), t.path().join("source"));
		for d in [&target, &source] {
			fs::create_dir_all(d.join("sub")).unwrap();
		}
		fs::write(target.join("a"), "A").unwrap();
		fs::write(target.join("sub/b"), "B").unwrap();
		fs::write(source.join("a"), "A edited").unwrap(); // still in the source, newer
		// sub/b was already removed from the source when the move stopped.

		let mut rest = Vec::new();
		merge_back(&Fs::unconfined(), &target, &source, "t", &mut |p| {
			rest.push(p.to_path_buf());
			Ok(())
		})
		.unwrap();
		assert_eq!(fs::read_to_string(source.join("a")).unwrap(), "A edited");
		assert_eq!(fs::read_to_string(source.join("sub/b")).unwrap(), "B");
		assert_eq!(rest, std::slice::from_ref(&target), "the stale duplicate goes to the trash, not away");
		assert!(target.join("a").exists());
	}

	/// What the recursive copy does below the top level: every nested entry is listed and created
	/// through the root's descriptor, so a folder swapped for a symlink mid-copy is not followed.
	#[test]
	fn nested_copy_steps_are_confined() {
		let t = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(t.path()).unwrap();
		let (root, outside) = (base.join("root"), base.join("outside"));
		fs::create_dir_all(root.join("src")).unwrap();
		fs::write(root.join("src/a"), "a").unwrap();
		fs::create_dir_all(outside.join("x")).unwrap();
		std::os::unix::fs::symlink(&outside, root.join("swapped")).unwrap();
		let fs_ = Fs::confined(std::slice::from_ref(&root));

		// Creating a nested entry under the swapped folder is refused…
		let err = fs_.copy_tree(&root.join("src/a"), &root.join("swapped/x/a"), Path::new("a"), &mut Vec::new());
		assert!(err.is_err());
		assert!(!outside.join("x/a").exists(), "wrote outside the root");
		// …and so is listing through it.
		assert!(fs_.list(&root.join("swapped/x")).is_err());
		assert_eq!(fs_.list(&root.join("src")).unwrap(), ["a"]);
	}

	/// The rollback of a half-done cross-device move is confined like everything else.
	#[test]
	fn merge_back_does_not_follow_a_planted_symlink() {
		let t = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(t.path()).unwrap();
		let (root, outside) = (base.join("root"), base.join("outside"));
		let (target, source) = (root.join("target"), root.join("source"));
		fs::create_dir_all(target.join("sub")).unwrap();
		fs::write(target.join("sub/authorized_keys"), "attacker").unwrap();
		fs::create_dir_all(&source).unwrap();
		fs::create_dir_all(&outside).unwrap();
		// The source's `sub` was removed by the move; the attacker plants a link in its place.
		std::os::unix::fs::symlink(&outside, source.join("sub")).unwrap();

		let fs_ = Fs::confined(std::slice::from_ref(&root));
		let result = merge_back(&fs_, &target, &source, "t", &mut |_| Ok(()));
		assert!(result.is_err());
		assert!(!outside.join("authorized_keys").exists(), "wrote through the planted link");
	}

	/// A symlink swapped in for a copied folder does not get that folder's mode.
	#[test]
	fn folder_mode_is_not_applied_through_a_symlink() {
		use std::os::unix::fs::PermissionsExt;
		let t = tempfile::TempDir::new().unwrap();
		let base = crate::path::canonicalize(t.path()).unwrap();
		let root = base.join("root");
		fs::create_dir_all(&root).unwrap();
		let victim = base.join("bashrc");
		fs::write(&victim, "x").unwrap();
		fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
		std::os::unix::fs::symlink(&victim, root.join("copied")).unwrap();

		let fs_ = Fs::confined(std::slice::from_ref(&root));
		assert!(fs_.set_dir_mode(&root.join("copied"), fs::Permissions::from_mode(0o777)).is_err());
		assert!(set_dir_mode(&root.join("copied"), fs::Permissions::from_mode(0o777)).is_err());
		assert_eq!(fs::metadata(&victim).unwrap().permissions().mode() & 0o777, 0o600);
	}

	/// A partial copy with read-only folders inside is still removed.
	#[test]
	fn partial_copies_with_read_only_folders_are_removed() {
		use std::os::unix::fs::PermissionsExt;
		let t = tempfile::TempDir::new().unwrap();
		let part = t.path().join(".tpfs-part-x");
		fs::create_dir_all(part.join("ro")).unwrap();
		fs::write(part.join("ro/f"), "f").unwrap();
		fs::set_permissions(part.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();
		remove_part(&Fs::unconfined(), &part).unwrap();
		assert!(!part.exists());
	}
}
