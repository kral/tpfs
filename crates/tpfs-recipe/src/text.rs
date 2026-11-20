//! Text rules: plain text, filters, file-name checks and name patterns.

/// A short, printable form of untrusted text for messages.
pub(crate) fn show(s: &str) -> String {
	let s: String = s.chars().take(80).collect();
	tpfs_core::sanitize(s.as_ref())
}

/// Bidirectional controls and other invisible formatting characters: they let a name read
/// differently from what it is.
pub(crate) fn is_invisible(c: char) -> bool {
	matches!(c, '\u{ad}' | '\u{34f}' | '\u{61c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
		| '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}')
}

/// Why `s` is not plain text of 1..=`max` characters, if it is not.
pub(crate) fn plain(s: &str, max: usize, newlines: bool) -> Option<String> {
	if s.trim().is_empty() {
		return Some("must not be empty".into());
	}
	if s.chars().count() > max {
		return Some(format!("at most {max} characters"));
	}
	if s.chars().any(|c| (c.is_control() && !(newlines && c == '\n')) || is_invisible(c)) {
		return Some("plain text only (no control or invisible characters)".into());
	}
	None
}

/// Why a scope root is not acceptable, if it is not.
pub(crate) fn root(r: &str) -> Option<String> {
	if r.is_empty() || r.len() > 1024 || r.chars().any(|c| c.is_control() || is_invisible(c)) {
		return Some("a folder path of at most 1024 printable characters".into());
	}
	let b = r.as_bytes();
	let absolute = r == "~"
		|| r.starts_with("~/")
		|| r.starts_with('/')
		|| r.starts_with("\\\\")
		|| (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/'));
	if !absolute {
		return Some("an absolute folder, or '~' for the home folder".into());
	}
	if r.split(['/', '\\']).any(|c| c == "..") {
		return Some("must not contain '..'".into());
	}
	None
}

pub(crate) fn regex(p: &str, max: usize) -> Result<regex::Regex, String> {
	if p.chars().count() > max {
		return Err(format!("at most {max} characters"));
	}
	regex::RegexBuilder::new(p)
		.size_limit(1 << 20)
		.dfa_size_limit(1 << 20)
		.build()
		.map_err(|e| format!("not a valid pattern: {}", show(&e.to_string())))
}

pub(crate) fn lower(s: &str) -> String {
	s.to_lowercase()
}

/// Diacritics stripped and a few letters spelled out (`č` → `c`, `ß` → `ss`); characters with no
/// ASCII form are dropped.
pub(crate) fn ascii(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for c in s.chars() {
		if c.is_ascii() {
			out.push(c);
		} else if let Ok(i) = crate::ascii::ASCII.binary_search_by_key(&c, |(k, _)| *k) {
			out.push_str(crate::ascii::ASCII[i].1);
		}
	}
	out
}

/// Spaces and characters a file name cannot hold on some system become `_`, repeats collapsed.
pub(crate) fn slug(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for c in s.chars() {
		let bad = c.is_whitespace()
			|| c.is_control()
			|| is_invisible(c)
			|| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|');
		let c = if bad { '_' } else { c };
		if !(c == '_' && out.ends_with('_')) {
			out.push(c);
		}
	}
	out
}

/// Why a produced name cannot be used, if it cannot. Applied to every path component a template
/// produces, on every system (a recipe must not depend on where it runs).
pub(crate) fn bad_component(name: &str) -> Option<&'static str> {
	if name.is_empty() {
		return Some("is empty");
	}
	if name == "." || name == ".." {
		return Some("is '.' or '..'");
	}
	if name.contains(['/', '\\']) {
		return Some("contains a path separator");
	}
	if name.chars().any(|c| c.is_control() || is_invisible(c)) {
		return Some("contains a control or invisible character");
	}
	if name.ends_with(['.', ' ']) {
		return Some("ends with a dot or a space");
	}
	if name.len() > 255 {
		return Some("is longer than 255 bytes");
	}
	if reserved(name) {
		return Some("is a reserved Windows device name");
	}
	if cfg!(windows) && name.contains([':', '*', '?', '"', '<', '>', '|']) {
		return Some("contains a character Windows does not allow");
	}
	None
}

/// `CON`, `nul.txt`, `COM1` …: names Windows treats as devices, with or without an extension.
fn reserved(name: &str) -> bool {
	let stem = name.split('.').next().unwrap_or_default().trim_end().to_ascii_uppercase();
	matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
		|| ((stem.starts_with("COM") || stem.starts_with("LPT"))
			&& stem.chars().count() == 4
			&& stem.chars().nth(3).is_some_and(|d| d.is_ascii_digit() || "¹²³".contains(d)))
}

/// `*` and `?` over a whole name, letter case ignored.
pub(crate) fn glob(pattern: &str, name: &str) -> bool {
	let p: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
	let n: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
	let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
	while j < n.len() {
		if i < p.len() && (p[i] == '?' || (p[i] != '*' && p[i] == n[j])) {
			i += 1;
			j += 1;
		} else if i < p.len() && p[i] == '*' {
			star = Some(i);
			mark = j;
			i += 1;
		} else if let Some(s) = star {
			i = s + 1;
			mark += 1;
			j = mark;
		} else {
			return false;
		}
	}
	p[i..].iter().all(|c| *c == '*')
}

/// `name` split into stem and extension at the last dot (a leading dot starts no extension).
pub(crate) fn split_ext(name: &str) -> (&str, &str) {
	match name.rfind('.') {
		Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
		_ => (name, ""),
	}
}
