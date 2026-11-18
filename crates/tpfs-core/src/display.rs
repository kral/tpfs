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

fn is_dangerous(ch: char) -> bool {
	ch.is_control()
		|| matches!(ch, '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
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
}
