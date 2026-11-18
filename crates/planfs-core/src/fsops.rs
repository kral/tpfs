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

	/// Rename that fails with `AlreadyExists` instead of replacing the target.
	pub(crate) fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
		#[cfg(target_os = "linux")]
		if let Some(result) = linux::rename(&self.anchors, from, to) {
			return result;
		}

		#[cfg(target_os = "linux")]
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

		if fs::symlink_metadata(to).is_ok() {
			return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("target exists: {}", to.display())));
		}
		fs::rename(from, to)
	}

	/// Move a file, directory or symlink; across file systems falls back to copy + verify + remove.
	pub(crate) fn move_any(&self, from: &Path, to: &Path) -> io::Result<()> {
		match self.rename_noreplace(from, to) {
			Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
				self.copy_any(from, to)?;
				if fs::symlink_metadata(from)?.is_dir() {
					fs::remove_dir_all(from)?;
				} else {
					fs::remove_file(from)?;
				}
				sync_parent(from);
				Ok(())
			}
			Ok(()) => {
				sync_parent(to);
				sync_parent(from);
				Ok(())
			}
			Err(e) => Err(e),
		}
	}

	/// Copy a file, directory (recursively) or symlink (as a symlink). Never overwrites.
	/// A top-level file inside a root is read and created through confined descriptors; the inside of
	/// a copied directory is walked by path.
	pub(crate) fn copy_any(&self, from: &Path, to: &Path) -> io::Result<()> {
		let meta = fs::symlink_metadata(from)?;
		let ft = meta.file_type();

		if ft.is_file() {
			#[cfg(target_os = "linux")]
			if let Some((src, dst)) = linux::open_copy_pair(&self.anchors, from, to, &meta)? {
				return copy_stream(src, dst, from, &meta);
			}
			let src = File::open(from)?;
			let dst = OpenOptions::new().write(true).create_new(true).open(to)?;
			copy_stream(src, dst, from, &meta)
		} else if ft.is_dir() {
			self.mkdir(to)?;
			for entry in fs::read_dir(from)? {
				let entry = entry?;
				Fs::unconfined().copy_any(&entry.path(), &to.join(entry.file_name()))?;
			}
			fs::set_permissions(to, meta.permissions())
		} else if ft.is_symlink() {
			copy_symlink(from, to)
		} else {
			Err(io::Error::new(io::ErrorKind::Unsupported, format!("special file: {}", from.display())))
		}
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
fn copy_symlink(from: &Path, to: &Path) -> io::Result<()> {
	std::os::unix::fs::symlink(fs::read_link(from)?, to)
}

#[cfg(not(unix))]
fn copy_symlink(from: &Path, _: &Path) -> io::Result<()> {
	Err(io::Error::new(io::ErrorKind::Unsupported, format!("symlink: {}", from.display())))
}

/// Make a directory entry change durable (best effort).
pub(crate) fn sync_parent(path: &Path) {
	#[cfg(unix)]
	if let Some(parent) = path.parent()
		&& let Ok(dir) = File::open(parent)
	{
		let _ = dir.sync_all();
	}
	#[cfg(not(unix))]
	let _ = path;
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
	#[cfg(not(unix))]
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
		let src = openat(sfd, sname, OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())?;

		let (dfd, dname) = side(&dst, to);
		let create = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
		let dst = openat(dfd, dname, create, Mode::from_raw_mode(meta.permissions().mode() & 0o7777))?;

		Ok(Some((File::from(src), File::from(dst))))
	}
}
