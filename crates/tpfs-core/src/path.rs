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

/// `s` in canonical decomposition (NFD) for Latin, Greek and Cyrillic letters: `é` (U+00E9) and
/// `e` + U+0301 give the same string, as macOS file systems treat them. Other scripts pass through
/// unchanged. Combining marks are put in canonical order.
pub fn nfd(s: &str) -> String {
	use crate::unicode::{COMBINING, DECOMPOSE};
	let mut out: Vec<char> = Vec::with_capacity(s.len());
	for c in s.chars() {
		match DECOMPOSE.binary_search_by_key(&c, |(k, _)| *k) {
			Ok(i) => out.extend(DECOMPOSE[i].1.chars()),
			Err(_) => out.push(c),
		}
	}
	let class = |c: char| COMBINING.binary_search_by_key(&c, |(k, _)| *k).map_or(0, |i| COMBINING[i].1);
	// Canonical ordering: a stable sort of each run of combining marks by their class.
	let mut i = 0;
	while i < out.len() {
		if class(out[i]) == 0 {
			i += 1;
			continue;
		}
		let start = i;
		while i < out.len() && class(out[i]) != 0 {
			i += 1;
		}
		out[start..i].sort_by_key(|c| class(*c));
	}
	out.into_iter().collect()
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

/// The network share a Windows UNC path is on (`\\server\share\`), which no drive letter covers.
/// `None` for other paths and on other systems.
pub fn share_root(p: &Path) -> Option<PathBuf> {
	match p.components().next()? {
		std::path::Component::Prefix(pre)
			if matches!(pre.kind(), std::path::Prefix::UNC(..) | std::path::Prefix::VerbatimUNC(..)) =>
		{
			let mut root = PathBuf::from(pre.as_os_str());
			root.push(std::path::MAIN_SEPARATOR_STR);
			Some(root)
		}
		_ => None,
	}
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

/// The non-verbatim spelling of a verbatim Windows path (drive and UNC paths), whatever its length:
/// one spelling everywhere, so long and short paths compare alike. Calls that need the verbatim form
/// for long paths add it back ([`win_api_path`]); std does that by itself.
#[cfg_attr(not(windows), allow(dead_code))]
fn strip_verbatim(s: &str) -> Option<String> {
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
	Some(out)
}

/// A path for a raw Win32 call: long absolute paths get the `\\?\` prefix the classic API needs.
#[cfg(windows)]
pub(crate) fn win_api_path(p: &Path) -> std::ffi::OsString {
	let s = p.as_os_str().to_string_lossy();
	if s.len() < 248 || s.starts_with(r"\\?\") {
		p.as_os_str().to_owned()
	} else if let Some(unc) = s.strip_prefix(r"\\") {
		format!(r"\\?\UNC\{unc}").into()
	} else {
		format!(r"\\?\{s}").into()
	}
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
		if !m.is_dir() || m.uid() != uid || m.permissions().mode() & 0o077 != 0 {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!(
					"refusing to use {}: it must be a real folder owned by you that only you can use (chmod 700)",
					dir.display()
				),
			));
		}
		// A folder above that anyone may change (and is not sticky, like /tmp), or that belongs to
		// another user, lets someone else swap the whole state folder for one of their own. Checked
		// on the real path: through a symlink the folders above its target are the ones that count.
		let real = canonicalize(dir)?;
		for parent in real.ancestors().skip(1) {
			let Ok(p) = fs::symlink_metadata(parent) else { continue };
			let why = if p.permissions().mode() & 0o1002 == 0o002 {
				"is writable by everyone"
			} else if p.uid() != 0 && p.uid() != uid {
				"belongs to another user"
			} else {
				continue;
			};
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!(
					"refusing to use {}: any user able to change {} could replace it ({} {why})",
					dir.display(),
					parent.display(),
					parent.display()
				),
			));
		}
		Ok(())
	}
	#[cfg(not(unix))]
	{
		// Windows: only a folder inside the user's own profile is private by default (the profile's ACL
		// grants nobody else access, and it is inherited); a folder elsewhere, such as `D:\tpfs` or
		// `C:\tpfs`, inherits "Authenticated Users: Modify" from the drive root, and could have been
		// created by another user. Such folders, and links, are refused.
		fs::create_dir_all(dir)?;
		let real = canonicalize(dir)?;
		let profile = std::env::var_os("USERPROFILE").and_then(|p| canonicalize(Path::new(&p)).ok());
		let inside = profile.is_some_and(|p| real.starts_with(&p) && real != p);
		if fs::symlink_metadata(dir)?.file_type().is_symlink() || !inside {
			return Err(io::Error::new(
				io::ErrorKind::PermissionDenied,
				format!(
					"refusing to use {}: the state folder must be a real folder inside your user profile (%USERPROFILE%), where only you can write",
					dir.display()
				),
			));
		}
		// And its own access list, protected from the profile's: only this user (and the system)
		// may use it, whatever the folders above allow. Files created inside inherit it.
		acl::make_private(dir).map_err(|e| {
			io::Error::new(io::ErrorKind::PermissionDenied, format!("cannot make {} private: {e}", dir.display()))
		})
	}
}

/// The Windows counterpart of mode 0700: a protected access list granting full control to the
/// current user and to SYSTEM only, inherited by everything inside.
#[cfg(windows)]
pub(crate) mod acl {
	use std::{io, os::windows::ffi::OsStrExt, path::Path, ptr};

	use windows_sys::Win32::{
		Foundation::{CloseHandle, HANDLE, LocalFree},
		Security::{
			ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, AclSizeInformation, AddAccessAllowedAceEx,
			Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW},
			CONTAINER_INHERIT_ACE, CreateWellKnownSid, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetAclInformation,
			GetLengthSid, GetSecurityDescriptorControl, GetTokenInformation, InitializeAcl, OBJECT_INHERIT_ACE,
			PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
			TokenUser, WinLocalSystemSid,
		},
		Storage::FileSystem::FILE_ALL_ACCESS,
		System::Threading::{GetCurrentProcess, OpenProcessToken},
	};

	fn wide(p: &Path) -> Vec<u16> {
		p.as_os_str().encode_wide().chain(Some(0)).collect()
	}

	/// The current user's SID, in a buffer that keeps it alive.
	fn user_sid() -> io::Result<Vec<u64>> {
		// SAFETY: the token handle is closed below; buffers are sized as the system asks.
		unsafe {
			let mut token: HANDLE = ptr::null_mut();
			if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
				return Err(io::Error::last_os_error());
			}
			let mut len = 0u32;
			GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len);
			let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
			let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
			CloseHandle(token);
			if ok == 0 {
				return Err(io::Error::last_os_error());
			}
			Ok(buf)
		}
	}

	fn sid_of_user(buf: &[u64]) -> *mut core::ffi::c_void {
		// SAFETY: `buf` holds a TOKEN_USER filled by GetTokenInformation.
		unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid }
	}

	fn system_sid() -> io::Result<Vec<u64>> {
		let mut size = 68u32; // SECURITY_MAX_SID_SIZE
		let mut buf = vec![0u64; 9];
		// SAFETY: `buf` has room for `size` bytes.
		if unsafe { CreateWellKnownSid(WinLocalSystemSid, ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size) } == 0 {
			return Err(io::Error::last_os_error());
		}
		Ok(buf)
	}

	/// Whether `dir` already has exactly the private access list (so it is not rewritten, which
	/// would walk everything inside).
	pub(crate) fn is_private(dir: &Path) -> io::Result<bool> {
		let user = user_sid()?;
		let system = system_sid()?;
		let name = wide(dir);
		// SAFETY: the descriptor is freed with LocalFree; the ACL points into it.
		unsafe {
			let mut dacl: *mut ACL = ptr::null_mut();
			let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
			let err = GetNamedSecurityInfoW(
				name.as_ptr() as _,
				SE_FILE_OBJECT,
				DACL_SECURITY_INFORMATION,
				ptr::null_mut(),
				ptr::null_mut(),
				&mut dacl,
				ptr::null_mut(),
				&mut sd,
			);
			if err != 0 {
				return Err(io::Error::from_raw_os_error(err as i32));
			}
			let mut control = 0u16;
			let mut revision = 0u32;
			let mut ok = !dacl.is_null()
				&& GetSecurityDescriptorControl(sd, &mut control, &mut revision) != 0
				&& control & SE_DACL_PROTECTED != 0;
			if ok {
				let mut info: ACL_SIZE_INFORMATION = std::mem::zeroed();
				ok = GetAclInformation(
					dacl,
					(&raw mut info).cast(),
					std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
					AclSizeInformation,
				) != 0 && info.AceCount == 2;
				for i in 0..2 {
					let mut ace: *mut core::ffi::c_void = ptr::null_mut();
					ok = ok && GetAce(dacl, i, &mut ace) != 0 && {
						let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
						let sid = (&raw mut (*ace).SidStart).cast::<core::ffi::c_void>();
						(*ace).Header.AceType == 0 // ACCESS_ALLOWED_ACE_TYPE
							&& (EqualSid(sid, sid_of_user(&user)) != 0 || EqualSid(sid, system.as_ptr() as _) != 0)
					};
				}
			}
			LocalFree(sd as _);
			Ok(ok)
		}
	}

	pub(crate) fn make_private(dir: &Path) -> io::Result<()> {
		if is_private(dir)? {
			return Ok(());
		}
		let user = user_sid()?;
		let system = system_sid()?;
		let sids = [sid_of_user(&user), system.as_ptr() as *mut core::ffi::c_void];
		// SAFETY: the ACL buffer is sized for both entries; the SIDs outlive the calls.
		unsafe {
			let entry = std::mem::size_of::<ACCESS_ALLOWED_ACE>() - std::mem::size_of::<u32>();
			let size =
				std::mem::size_of::<ACL>() + sids.iter().map(|s| entry + GetLengthSid(*s) as usize).sum::<usize>();
			let mut buf = vec![0u64; size.div_ceil(8)];
			let acl = buf.as_mut_ptr().cast::<ACL>();
			if InitializeAcl(acl, size as u32, ACL_REVISION) == 0 {
				return Err(io::Error::last_os_error());
			}
			for sid in sids {
				if AddAccessAllowedAceEx(
					acl,
					ACL_REVISION,
					OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
					FILE_ALL_ACCESS,
					sid,
				) == 0
				{
					return Err(io::Error::last_os_error());
				}
			}
			let name = wide(dir);
			let err = SetNamedSecurityInfoW(
				name.as_ptr() as _,
				SE_FILE_OBJECT,
				DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
				ptr::null_mut(),
				ptr::null_mut(),
				acl,
				ptr::null(),
			);
			if err != 0 {
				return Err(io::Error::from_raw_os_error(err as i32));
			}
		}
		Ok(())
	}

	#[cfg(test)]
	mod tests {
		#[test]
		fn the_state_folder_gets_a_private_access_list() {
			let tmp = tempfile::TempDir::new().unwrap();
			let dir = tmp.path().join("state");
			crate::private_dir(&dir).unwrap();
			assert!(super::is_private(&dir).unwrap());
			crate::private_dir(&dir).unwrap(); // already private: left as it is
			std::fs::write(dir.join("f"), "x").unwrap();
			std::fs::read(dir.join("f")).unwrap();
		}
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

/// Identity of the file system (volume) holding `path`, judged on the path itself (a final symlink
/// is not followed) or its nearest existing ancestor. On Windows: the drive or share prefix.
/// The one helper for "same file system?" questions in validation, risk and the trash.
pub(crate) fn device_of(path: &Path) -> Option<u64> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::MetadataExt;
		path.ancestors().find_map(|a| fs::symlink_metadata(a).ok()).map(|m| m.dev())
	}
	#[cfg(windows)]
	{
		use std::hash::{Hash, Hasher};
		let mut h = std::collections::hash_map::DefaultHasher::new();
		match path.components().next() {
			Some(std::path::Component::Prefix(pre)) => pre.as_os_str().to_ascii_uppercase().hash(&mut h),
			_ => return None,
		}
		Some(h.finish())
	}
	#[cfg(not(any(unix, windows)))]
	{
		let _ = path;
		None
	}
}

/// Whether `from` and `target_dir` are on different file systems (unknown counts as the same).
pub(crate) fn crosses_devices(from: &Path, target_dir: &Path) -> bool {
	match (device_of(from), device_of(target_dir)) {
		(Some(a), Some(b)) => a != b,
		_ => false,
	}
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
		assert_eq!(strip_verbatim(&long).map(|s| s.len()), Some(long.len() - 4));
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

#[cfg(test)]
mod nfd_tests {
	use super::nfd;

	#[test]
	fn composed_and_decomposed_names_are_one() {
		assert_eq!(nfd("Účetnictví"), nfd("U\u{301}c\u{30c}etnictvi\u{301}"));
		assert_eq!(nfd("Café Müller"), "Cafe\u{301} Mu\u{308}ller");
		assert_eq!(nfd("ǖ"), "u\u{308}\u{304}", "decomposed fully");
		assert_eq!(nfd("q\u{307}\u{323}"), nfd("q\u{323}\u{307}"), "marks in canonical order");
		assert_eq!(nfd("Ἀθῆναι Москва́"), nfd("Ἀθῆναι Москва\u{301}"));
		assert_eq!(nfd("plain.txt"), "plain.txt");
		assert_ne!(nfd("é"), nfd("e"));
	}
}
