//! Small helpers: local time formatting, the F3 viewer's content, Nerd Font icons.

use std::{
	fs::File,
	io::Read,
	path::Path,
	time::{SystemTime, UNIX_EPOCH},
};

/// Places to jump to with Alt+F1 / Alt+F2: every drive on Windows; elsewhere `/`, the home folder
/// and mounted removable media (`/media/<user>/*`, `/run/media/<user>/*`, `/mnt/*`).
pub(crate) fn drives() -> Vec<std::path::PathBuf> {
	let mut out = tpfs_core::system_roots();
	#[cfg(unix)]
	{
		use std::path::PathBuf;
		if let Some(home) = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_dir()) {
			out.push(home);
		}
		let user = std::env::var_os("USER").unwrap_or_default();
		let mut dirs = vec![PathBuf::from("/mnt")];
		if !user.is_empty() {
			dirs.push(PathBuf::from("/media").join(&user));
			dirs.push(PathBuf::from("/run/media").join(&user));
		}
		for dir in dirs {
			let Ok(rd) = std::fs::read_dir(&dir) else { continue };
			let mut found: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()).collect();
			found.sort();
			out.extend(found);
		}
	}
	out.dedup();
	out
}

/// `YY-MM-DD HH:MM` in local time (UTC where local time is unavailable).
pub(crate) fn format_time(t: SystemTime) -> (String, String) {
	let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
	let (y, mo, d, h, mi) = local(secs);
	(format!("{:02}-{mo:02}-{d:02}", y % 100), format!("{h:02}:{mi:02}"))
}

#[cfg(unix)]
fn local(secs: i64) -> (i64, u32, u32, u32, u32) {
	let t = secs as libc::time_t;
	// SAFETY: `tm` holds only integers and (on some systems) a `tm_zone` pointer, for which all
	// zeroes (null) is a valid value.
	let mut tm: libc::tm = unsafe { std::mem::zeroed() };
	// SAFETY: both pointers are valid for the call; localtime_r is the thread-safe variant and only
	// writes `tm`. It reads TZ from the environment, which tpfs never modifies (no setenv).
	if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
		return utc(secs);
	}
	(tm.tm_year as i64 + 1900, tm.tm_mon as u32 + 1, tm.tm_mday as u32, tm.tm_hour as u32, tm.tm_min as u32)
}

#[cfg(windows)]
fn local(secs: i64) -> (i64, u32, u32, u32, u32) {
	let t = secs as libc::time_t;
	// SAFETY: `tm` holds only integers, for which all zeroes is a valid value.
	let mut tm: libc::tm = unsafe { std::mem::zeroed() };
	// SAFETY: both pointers are valid for the call; localtime_s only writes `tm`.
	if unsafe { libc::localtime_s(&mut tm, &t) } != 0 {
		return utc(secs);
	}
	(tm.tm_year as i64 + 1900, tm.tm_mon as u32 + 1, tm.tm_mday as u32, tm.tm_hour as u32, tm.tm_min as u32)
}

#[cfg(not(any(unix, windows)))]
fn local(secs: i64) -> (i64, u32, u32, u32, u32) {
	utc(secs)
}

/// Civil date from Unix time (Howard Hinnant's algorithm).
fn utc(secs: i64) -> (i64, u32, u32, u32, u32) {
	let days = secs.div_euclid(86_400);
	let rem = secs.rem_euclid(86_400);
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z - era * 146_097;
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
	let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
	let y = yoe + era * 400 + i64::from(m <= 2);
	(y, m, d, (rem / 3600) as u32, (rem % 3600 / 60) as u32)
}

/// Lines for the F3 viewer: sanitised text, or a hex dump for binary content. Reads at most 256 KiB.
pub(crate) fn view_lines(path: &Path) -> Result<Vec<String>, String> {
	const LIMIT: u64 = 256 * 1024;
	let mut buf = Vec::new();
	// Only regular files (a link to one is fine): a FIFO or device named like a text file must
	// not hang the commander.
	if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
		return Err("not a regular file".into());
	}
	File::open(path).and_then(|f| f.take(LIMIT).read_to_end(&mut buf)).map_err(|e| e.to_string())?;

	let binary = buf.contains(&0) || std::str::from_utf8(&buf).is_err() && buf.len() < 4096;
	let mut lines: Vec<String> = if binary {
		buf.chunks(16)
			.enumerate()
			.map(|(i, chunk)| {
				let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
				let ascii: String =
					chunk.iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '.' }).collect();
				format!("{:08x}  {hex:<48} {ascii}", i * 16)
			})
			.collect()
	} else {
		String::from_utf8_lossy(&buf).lines().map(|l| tpfs_core::sanitize(l.replace('\t', "    ").as_ref())).collect()
	};
	if buf.len() as u64 == LIMIT {
		lines.push(format!("… (only the first {} KiB are shown)", LIMIT / 1024));
	}
	Ok(lines)
}

/// Nerd Font glyph for an entry (shown only with `--icons`).
pub(crate) fn icon(name: &str, is_dir: bool, is_up: bool) -> char {
	if is_up {
		return '\u{f148}';
	}
	if is_dir {
		return '\u{f07b}';
	}
	let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
	match ext.as_str() {
		"rs" | "ts" | "tsx" | "js" | "py" | "go" | "c" | "h" | "cpp" | "java" | "cs" | "lua" | "json" | "toml"
		| "yaml" | "yml" => '\u{f121}',
		"png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp" | "heic" => '\u{f1c5}',
		"zip" | "gz" | "tgz" | "xz" | "zst" | "7z" | "rar" | "tar" | "bz2" | "iso" => '\u{f1c6}',
		"pdf" => '\u{f1c1}',
		"xls" | "xlsx" | "ods" | "csv" => '\u{f1c3}',
		"doc" | "docx" | "odt" | "rtf" => '\u{f1c2}',
		"mp3" | "flac" | "wav" | "ogg" | "m4a" => '\u{f1c7}',
		"mp4" | "mkv" | "mov" | "avi" | "webm" => '\u{f1c8}',
		"sh" | "bash" | "zsh" | "exe" | "bat" | "ps1" => '\u{f120}',
		"md" | "txt" | "log" => '\u{f15c}',
		_ => '\u{f15b}',
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn civil_date_from_unix_time() {
		assert_eq!(utc(0), (1970, 1, 1, 0, 0));
		assert_eq!(utc(1_763_455_500), (2025, 11, 18, 8, 45));
		assert_eq!(utc(951_782_400), (2000, 2, 29, 0, 0));
	}

	#[test]
	fn viewer_sanitises_text_and_dumps_binary() {
		let tmp = tempfile::TempDir::new().unwrap();
		let t = tmp.path().join("t.txt");
		std::fs::write(&t, "ok\n\x1b[2Jclear").unwrap();
		assert_eq!(view_lines(&t).unwrap(), ["ok", "\\u{1b}[2Jclear"]);

		let b = tmp.path().join("b.bin");
		std::fs::write(&b, [0u8, 1, 2, b'A']).unwrap();
		assert!(view_lines(&b).unwrap()[0].starts_with("00000000  00 01 02 41"));
	}
}
