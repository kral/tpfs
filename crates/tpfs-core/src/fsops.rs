//! File operations that never overwrite anything.
//!
//! On Linux, paths inside the allowed roots are not resolved by path: the parent directory is opened
//! from the root's directory descriptor with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`, and the
//! operation runs relative to it (`renameat2`, `mkdirat`, `unlinkat`, `openat`). A symlink swapped into
//! the path after validation therefore makes the operation fail instead of redirecting it. Paths outside
//! the roots (the trash) and other platforms use plain path-based calls.

use std::{
	fs::{self, File, OpenOptions},
	io,
	path::{Path, PathBuf},
};

pub(crate) struct Fs {
	#[cfg(target_os = "linux")]
	anchors: Vec<linux::Anchor>,
}

impl Fs {
	/// Plain path-based operations (undo and recovery replay inverses that may lead anywhere).
	pub(crate) fn unconfined() -> Self {
		Self {
			#[cfg(target_os = "linux")]
			anchors: Vec::new(),
		}
	}

	/// Operations under any of `roots` are confined to it; roots are expected to be canonical.
	pub(crate) fn confined(roots: &[PathBuf]) -> Self {
		#[cfg(target_os = "linux")]
		{
			Self { anchors: roots.iter().filter_map(|r| linux::Anchor::open(r)).collect() }
		}
		#[cfg(not(target_os = "linux"))]
		{
			let _ = roots;
			Self::unconfined()
		}
	}

	/// Rename that fails with `AlreadyExists` instead of replacing the target, atomically where the
	/// platform can: `renameat2(RENAME_NOREPLACE)` on Linux, `renameatx_np(RENAME_EXCL)` on macOS,
	/// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING` on Windows. Only a file system without such
	/// support (some network and FUSE file systems) falls back to check-then-rename.
	pub(crate) fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
		#[cfg(target_os = "linux")]
		if let Some(result) = linux::rename(&self.anchors, from, to) {
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
		#[cfg(target_os = "linux")]
		if let Some(result) = linux::unlink(&self.anchors, path) {
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
		#[cfg(target_os = "linux")]
		if let Some(result) = linux::list(&self.anchors, dir) {
			return result;
		}
		fs::read_dir(dir)?.map(|e| e.map(|e| e.file_name())).collect()
	}

	fn symlink(&self, target: &Path, path: &Path) -> io::Result<()> {
		#[cfg(target_os = "linux")]
		if let Some(result) = linux::symlink(&self.anchors, target, path) {
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
				let left = remove_copied(self, from, &copied)?;
				sync_parent(from);
				if left.is_empty() {
					Ok(())
				} else {
					Err(io::Error::other(format!(
						"{} item(s) changed or appeared in {} during the move and were left there, e.g. {}",
						left.len(),
						from.display(),
						left[0].display()
					)))
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

	/// Copy `from` to `to` through `part`: the copy only ever appears at `to` complete.
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
		let result = self.copy_tree(from, part, Path::new(""), &mut copied);
		let result = result.and_then(|()| {
			sync_tree_dirs(part, &copied);
			self.rename_noreplace(part, to)
		});
		if let Err(e) = result {
			// The partial copy is ours and the source is untouched: drop it.
			remove_part(part);
			return Err(e);
		}
		sync_parent(to);
		publish()?;
		Ok(copied)
	}

	/// Copy a file, directory (recursively) or symlink (as a symlink), recording what was copied.
	/// Never overwrites. On Linux every entry under a root, at any depth, is listed, read and
	/// created through descriptors confined to that root.
	fn copy_tree(&self, from: &Path, to: &Path, rel: &Path, copied: &mut Vec<Copied>) -> io::Result<()> {
		let meta = fs::symlink_metadata(from)?;
		let ft = meta.file_type();

		if ft.is_file() {
			#[cfg(target_os = "linux")]
			if let Some((src, dst)) = linux::open_copy_pair(&self.anchors, from, to, &meta)? {
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
			for name in self.list(from)? {
				self.copy_tree(&from.join(&name), &to.join(&name), &rel.join(&name), copied)?;
			}
			return fs::set_permissions(to, meta.permissions());
		} else if ft.is_symlink() {
			self.symlink(&fs::read_link(from)?, to)?;
		} else {
			return Err(io::Error::new(io::ErrorKind::Unsupported, format!("special file: {}", from.display())));
		}
		copied.push(Copied::of(rel, &meta));
		Ok(())
	}

	pub(crate) fn mkdir(&self, path: &Path) -> io::Result<()> {
		#[cfg(target_os = "linux")]
		if let Some(result) = linux::mkdir(&self.anchors, path) {
			return result;
		}
		fs::create_dir(path)
	}

	pub(crate) fn rmdir(&self, path: &Path) -> io::Result<()> {
		#[cfg(target_os = "linux")]
		if let Some(result) = linux::rmdir(&self.anchors, path) {
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
	dst.set_permissions(meta.permissions())?;
	dst.sync_all()
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
/// before parents. Returns what was left in place (changed, new, or a folder that is not empty).
fn remove_copied(fs: &Fs, from: &Path, copied: &[Copied]) -> io::Result<Vec<PathBuf>> {
	let mut left = Vec::new();
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
		let removed = if c.dir { fs.rmdir(&path) } else { fs.remove_file(&path) };
		if removed.is_err() {
			left.push(path);
		}
	}
	Ok(left)
}

/// Make the directory entries of a freshly copied tree durable before it is published.
fn sync_tree_dirs(root: &Path, copied: &[Copied]) {
	for c in copied.iter().filter(|c| c.dir) {
		let dir = if c.rel.as_os_str().is_empty() { root.to_path_buf() } else { root.join(&c.rel) };
		sync_dir(&dir);
	}
}

/// Delete a partial copy that only tpfs ever saw (its private `part` name).
pub(crate) fn remove_part(part: &Path) {
	match fs::symlink_metadata(part) {
		Ok(m) if m.is_dir() => {
			let _ = fs::remove_dir_all(part);
		}
		Ok(_) => {
			let _ = fs::remove_file(part);
		}
		Err(_) => {}
	}
}

/// The private name a step's result is built under, next to its target.
pub(crate) fn part_path(to: &Path, tag: &str) -> PathBuf {
	to.with_file_name(format!(".tpfs-part-{tag}"))
}

/// Put back into `source` what a cross-device move had already removed from it, taking it from the
/// published copy at `target`; then move the rest of `target` (duplicates of what is still in
/// `source`) to `trash_rest`. Used when such a move stopped half way.
pub(crate) fn merge_back(
	target: &Path,
	source: &Path,
	trash_rest: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
	let fs = Fs::unconfined();
	let mut entries = Vec::new();
	collect(target, Path::new(""), &mut entries)?;
	for (rel, is_dir) in &entries {
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
		let tgt = target.join(rel);
		let part = part_path(&src, "restore");
		fs.move_any(&tgt, &src, &part, &mut || Ok(()))?;
	}
	// Folders now empty were fully restored; whatever is left duplicates the source.
	for (rel, _) in entries.iter().rev().filter(|(_, d)| *d) {
		let _ = fs::remove_dir(target.join(rel));
	}
	if crate::path::exists(target) { trash_rest(target) } else { Ok(()) }
}

fn collect(path: &Path, rel: &Path, out: &mut Vec<(PathBuf, bool)>) -> io::Result<()> {
	let is_dir = fs::symlink_metadata(path)?.is_dir();
	out.push((rel.to_path_buf(), is_dir));
	if is_dir {
		for entry in fs::read_dir(path)? {
			let name = entry?.file_name();
			collect(&path.join(&name), &rel.join(&name), out)?;
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
	#[cfg(unix)]
	if let Ok(d) = File::open(dir) {
		let _ = d.sync_all();
	}
	#[cfg(not(unix))]
	let _ = dir;
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

#[cfg(target_os = "linux")]
mod linux {
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

	use rustix::{
		fs::{
			AtFlags, CWD, Mode, OFlags, RenameFlags, ResolveFlags, mkdirat, openat, openat2, renameat_with, unlinkat,
		},
		io::Errno,
	};

	pub(super) struct Anchor {
		root: PathBuf,
		fd: OwnedFd,
	}

	impl Anchor {
		pub(super) fn open(root: &Path) -> Option<Self> {
			let fd = rustix::fs::open(root, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).ok()?;
			Some(Self { root: root.to_path_buf(), fd })
		}
	}

	const RESOLVE: ResolveFlags =
		ResolveFlags::BENEATH.union(ResolveFlags::NO_SYMLINKS).union(ResolveFlags::NO_MAGICLINKS);

	/// A path inside a root: its parent directory opened beneath the root, and its final name.
	/// `None` = not under any root, or `openat2` unsupported (kernel < 5.6): use path-based calls.
	fn locate<'p>(anchors: &[Anchor], path: &'p Path) -> Option<io::Result<(OwnedFd, &'p OsStr)>> {
		let anchor = anchors.iter().filter(|a| path.starts_with(&a.root)).max_by_key(|a| a.root.as_os_str().len())?;
		let rel = path.strip_prefix(&anchor.root).ok()?;
		let name = rel.file_name()?;
		let parent = rel.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));

		match openat2(&anchor.fd, parent, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty(), RESOLVE) {
			Ok(fd) => Some(Ok((fd, name))),
			Err(Errno::NOSYS) => None,
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

	pub(super) fn unlink(anchors: &[Anchor], path: &Path) -> Option<io::Result<()>> {
		Some(locate(anchors, path)?.and_then(|(fd, name)| Ok(unlinkat(&fd, name, AtFlags::empty())?)))
	}

	pub(super) fn symlink(anchors: &[Anchor], target: &Path, path: &Path) -> Option<io::Result<()>> {
		Some(locate(anchors, path)?.and_then(|(fd, name)| Ok(rustix::fs::symlinkat(target, &fd, name)?)))
	}

	/// List a directory opened beneath its root without following symlinks.
	pub(super) fn list(anchors: &[Anchor], dir: &Path) -> Option<io::Result<Vec<std::ffi::OsString>>> {
		use std::os::unix::ffi::OsStrExt;
		let anchor = anchors.iter().filter(|a| dir.starts_with(&a.root)).max_by_key(|a| a.root.as_os_str().len())?;
		let rel = dir.strip_prefix(&anchor.root).ok()?;
		let rel = if rel.as_os_str().is_empty() { Path::new(".") } else { rel };
		let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
		let fd = match openat2(&anchor.fd, rel, flags, Mode::empty(), RESOLVE) {
			Ok(fd) => fd,
			Err(Errno::NOSYS) => return None,
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
			for entry in rustix::fs::Dir::read_from(&fd)? {
				let entry = entry?;
				let name = entry.file_name().to_bytes();
				if name != b"." && name != b".." {
					names.push(OsStr::from_bytes(name).to_os_string());
				}
			}
			Ok(names)
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
		let dst = openat(dfd, dname, create, Mode::from_raw_mode(meta.permissions().mode() & 0o7777))?;

		Ok(Some((File::from(src), File::from(dst))))
	}
}

#[cfg(windows)]
mod windows {
	use std::{io, os::windows::ffi::OsStrExt, path::Path};

	use windows_sys::Win32::{
		Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
		Storage::FileSystem::{
			BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
			FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle, MOVEFILE_WRITE_THROUGH,
			MoveFileExW, OPEN_EXISTING,
		},
	};

	/// (volume serial, file index): the identity of a file, whatever name reaches it.
	pub(super) fn file_id(p: &Path) -> Option<(u32, u64)> {
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
		// SAFETY: `h` is an open handle and `info` a valid out-pointer; the handle is closed once.
		let ok = unsafe { GetFileInformationByHandle(h, &mut info) };
		// SAFETY: `h` was returned by CreateFileW above and is not used after this.
		unsafe { CloseHandle(h) };
		(ok != 0).then(|| {
			(info.dwVolumeSerialNumber, (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow))
		})
	}

	fn wide(p: &Path) -> Vec<u16> {
		p.as_os_str().encode_wide().chain(Some(0)).collect()
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
		assert!(err.to_string().contains("left there"), "{err}");
		assert_eq!(fs::read_to_string(&late).unwrap(), "written meanwhile");
		assert_eq!(fs::read_to_string(to.join("photo.jpg")).unwrap(), "photo");
		assert!(!from.join("photo.jpg").exists());

		// Rolling back puts the moved file home and keeps the late one.
		let mut rest = Vec::new();
		merge_back(&to, &from, &mut |p| {
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
		let Some((a, b)) = two_devices() else { return };
		let from = a.path().join("dir");
		fs::create_dir(&from).unwrap();
		fs::write(from.join("x"), "x").unwrap();
		fs::create_dir(from.join("sub")).unwrap();
		let to = b.path().join("dir");
		let part = part_path(&to, "t");

		// Publishing fails (as if the journal could not be written): no target, no part.
		let err = Fs::unconfined().copy_to(&from, &to, &part, &mut || {
			assert!(to.join("sub").is_dir() && !part.exists());
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
		merge_back(&target, &source, &mut |p| {
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
}
