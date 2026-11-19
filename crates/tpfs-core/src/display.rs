use std::{ffi::OsStr, fmt::Write};

/// Make a file name safe to print to a terminal or show in a UI.
///
/// File names may contain anything except `/` and NUL. Printed raw, control characters can inject
/// terminal escape sequences (rewrite the screen, set the clipboard via OSC 52, …) and bidi controls
/// can make a name read differently than it is ("Trojan Source"). Every such character, and every
/// byte that is not valid UTF-8, is shown as a visible escape instead.
pub fn sanitize(name: &OsStr) -> String {
	let mut out = String::with_capacity(name.len());
	for chunk in name.as_encoded_bytes().utf8_chunks() {
		for ch in chunk.valid().chars() {
			if is_dangerous(ch) {
				let _ = write!(out, "\\u{{{:x}}}", ch as u32);
			} else if ch == '\\' && cfg!(unix) {
				// On Unix a backslash is an ordinary name character: escape it, so a name that
				// literally contains `\u{202e}` cannot pass for one with the real character.
				out.push_str("\\\\");
			} else {
				out.push(ch);
			}
		}
		for b in chunk.invalid() {
			let _ = write!(out, "\\x{b:02x}");
		}
	}
	out
}

/// Control characters, line/paragraph separators, and invisible format characters (Unicode
/// category Cf: bidi controls, zero-width characters, tags…) that make a name look different from
/// what it is.
fn is_dangerous(ch: char) -> bool {
	ch.is_control()
		|| matches!(
			ch,
			'\u{ad}'
				| '\u{600}'..='\u{605}'
				| '\u{61c}'
				| '\u{6dd}'
				| '\u{70f}'
				| '\u{890}'..='\u{891}'
				| '\u{8e2}'
				| '\u{180e}'
				| '\u{200b}'..='\u{200f}'
				| '\u{2028}'..='\u{202e}'
				| '\u{2060}'..='\u{2064}'
				| '\u{2066}'..='\u{206f}'
				| '\u{feff}'
				| '\u{fff9}'..='\u{fffb}'
				| '\u{110bd}'
				| '\u{110cd}'
				| '\u{13430}'..='\u{1343f}'
				| '\u{1bca0}'..='\u{1bca3}'
				| '\u{1d173}'..='\u{1d17a}'
				| '\u{e0001}'
				| '\u{e0020}'..='\u{e007f}'
		)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn escapes_controls_and_bidi() {
		assert_eq!(sanitize(OsStr::new("ok name.txt")), "ok name.txt");
		assert_eq!(sanitize(OsStr::new("a\x1b]52;c;evil\x07b")), "a\\u{1b}]52;c;evil\\u{7}b");
		assert_eq!(sanitize(OsStr::new("invoice\u{202e}fdp.exe")), "invoice\\u{202e}fdp.exe");
		assert_eq!(sanitize(OsStr::new("line\nbreak")), "line\\u{a}break");
	}

	#[cfg(unix)]
	#[test]
	fn escapes_invalid_utf8() {
		use std::os::unix::ffi::OsStrExt;
		assert_eq!(sanitize(OsStr::from_bytes(b"caf\xe9")), "caf\\xe9");
	}

	#[test]
	fn invisible_characters_are_shown() {
		for name in ["a\u{200b}b", "a\u{2028}b", "a\u{feff}b", "a\u{e0041}b", "a\u{ad}b"] {
			let shown = sanitize(OsStr::new(name));
			assert!(shown.contains("\\u{"), "{name:?} -> {shown}");
		}
	}
}
