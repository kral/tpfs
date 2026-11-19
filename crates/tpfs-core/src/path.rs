use std::{
	fs, io,
	path::{Component, Path, PathBuf},
};

/// Lexically normalise an absolute path: resolve `.` and `..` without touching the disk.
/// Returns `None` for relative paths.
pub fn normalize(path: &Path) -> Option<PathBuf> {
	if !path.is_absolute() {
		return None;
	}

	let mut out = PathBuf::new();
	for c in path.components() {
		match c {
			Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
			Component::CurDir => {}
			Component::ParentDir => {
				out.pop();
			}
			Component::Normal(n) => out.push(n),
		}
	}
	Some(out)
}

/// `fs::canonicalize` without the Windows verbatim prefix: `\\?\C:\x` becomes `C:\x` and
/// `\\?\UNC\srv\share` becomes `\\srv\share`, the form users type, see and that other paths
/// are compared against. Elsewhere this is plain `fs::canonicalize`.
pub fn canonicalize(path: &Path) -> io::Result<PathBuf> {
	let p = fs::canonicalize(path)?;
	#[cfg(windows)]
	let p = match p.to_str().and_then(strip_verbatim) {
		Some(s) => PathBuf::from(s),
		None => p,
	};
	Ok(p)
}

/// Roots that cover the whole file system: `/` on Unix, every existing drive on Windows.
/// For callers that act on the user's behalf anywhere they can browse (commander, GUI, undo).
pub fn system_roots() -> Vec<PathBuf> {
	#[cfg(windows)]
	{
		(b'A'..=b'Z').map(|d| PathBuf::from(format!("{}:\\", d as char))).filter(|r| r.is_dir()).collect()
	}
	#[cfg(not(windows))]
	{
		vec![PathBuf::from("/")]
	}
}

/// The non-verbatim spelling of a verbatim Windows path, if it has one that means the same thing:
/// only drive and UNC paths short enough for the classic API (verbatim is still needed beyond).
#[cfg_attr(not(windows), allow(dead_code))]
fn strip_verbatim(s: &str) -> Option<String> {
	const MAX: usize = 259;
	let out = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
		format!(r"\\{rest}")
	} else {
		let rest = s.strip_prefix(r"\\?\")?;
		let b = rest.as_bytes();
		if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || b[2] != b'\\' {
			return None;
		}
		rest.to_string()
	};
	(out.len() <= MAX).then_some(out)
}

/// Create (if needed) a directory only the current user can use, and refuse one that is not:
/// a symlink, owned by someone else, or writable by group or others. tpfs keeps its journal,
/// trash and approvals in such directories; anyone able to write there could forge them.
pub fn private_dir(dir: &Path) -> io::Result<()> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
		fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
		let m = fs::symlink_metadata(dir)?;
		let uid = rustix::process::geteuid().as_raw();
		if !m.is_dir() || m.uid() != uid || m.permissions().mode() & 0o022 != 0 {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!(
					"refusing to use {}: it must be a real folder owned by you and not writable by others (chmod 700)",
					dir.display()
				),
			));
		}
		Ok(())
	}
	#[cfg(not(unix))]
	{
		// Windows: the folder inherits the ACL of the user's profile; refuse a reparse point.
		fs::create_dir_all(dir)?;
		if fs::symlink_metadata(dir)?.file_type().is_symlink() {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!("refusing to use {}: it is a link", dir.display()),
			));
		}
		Ok(())
	}
}

/// Refuse a private file that is a symlink, or (Unix) not owned by the user or readable by others.
pub fn check_private_file(path: &Path) -> io::Result<()> {
	let m = match fs::symlink_metadata(path) {
		Ok(m) => m,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
		Err(e) => return Err(e),
	};
	let bad = m.file_type().is_symlink() || !m.is_file();
	#[cfg(unix)]
	let bad = bad || {
		use std::os::unix::fs::{MetadataExt, PermissionsExt};
		m.uid() != rustix::process::geteuid().as_raw() || m.permissions().mode() & 0o077 != 0
	};
	if bad {
		return Err(io::Error::new(
			io::ErrorKind::PermissionDenied,
			format!("refusing to use {}: it must be a regular file owned by you, private (chmod 600)", path.display()),
		));
	}
	Ok(())
}

/// Whether something exists at `path` without following a final symlink.
pub(crate) fn exists(path: &Path) -> bool {
	fs::symlink_metadata(path).is_ok()
}

/// The first symlink among the directories between `root` (exclusive) and `path` (exclusive),
/// i.e. a component that would redirect the operation somewhere else. The final component itself
/// may be a symlink: operations act on the link, not on its target.
pub(crate) fn symlink_in_between(path: &Path, root: &Path) -> io::Result<Option<PathBuf>> {
	let mut dirs: Vec<&Path> = path.ancestors().skip(1).take_while(|a| a.starts_with(root) && *a != root).collect();
	dirs.reverse();

	for dir in dirs {
		match fs::symlink_metadata(dir) {
			Ok(m) if m.file_type().is_symlink() => return Ok(Some(dir.to_path_buf())),
			Ok(_) => {}
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
			Err(e) => return Err(e),
		}
	}
	Ok(None)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn strips_verbatim_prefix() {
		assert_eq!(strip_verbatim(r"\\?\C:\Users\me").as_deref(), Some(r"C:\Users\me"));
		assert_eq!(strip_verbatim(r"\\?\C:\").as_deref(), Some(r"C:\"));
		assert_eq!(strip_verbatim(r"\\?\UNC\nas\photos").as_deref(), Some(r"\\nas\photos"));
		assert_eq!(strip_verbatim(r"\\?\Volume{abc}\x"), None);
		assert_eq!(strip_verbatim(r"C:\already"), None);
		let long = format!(r"\\?\C:\{}", "a".repeat(300));
		assert_eq!(strip_verbatim(&long), None);
	}

	#[test]
	#[cfg(unix)]
	fn normalizes_dots() {
		assert_eq!(normalize(Path::new("/a/./b/../c")), Some(PathBuf::from("/a/c")));
		assert_eq!(normalize(Path::new("/../..")), Some(PathBuf::from("/")));
		assert_eq!(normalize(Path::new("relative")), None);
	}

	#[test]
	#[cfg(windows)]
	fn normalizes_dots() {
		assert_eq!(normalize(Path::new(r"C:\a\.\b\..\c")), Some(PathBuf::from(r"C:\a\c")));
		assert_eq!(normalize(Path::new(r"C:\..\..")), Some(PathBuf::from(r"C:\")));
		assert_eq!(normalize(Path::new(r"\\nas\photos\x\..\y")), Some(PathBuf::from(r"\\nas\photos\y")));
		// No drive: rooted but not absolute on Windows.
		assert_eq!(normalize(Path::new(r"\a\b")), None);
		assert_eq!(normalize(Path::new("relative")), None);
	}
}
