//! Target templates: `{source[:format][|filter…]}` between literal text, `/` between path components.

use crate::{ActionOp, Field, FormSchema, MAX_TEMPLATE, StringFormat, text};

/// Where a move or copy target starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Base {
	/// The selected entry's own folder (also when the template names no folder).
	ItemDir,
	/// A `format: "x-tpfs-dir"` form field.
	Form(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Source {
	Name,
	Stem,
	Ext,
	Mtime,
	Today,
	N,
	Form(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DatePart {
	Lit(String),
	Year,
	Month,
	Day,
	Hour,
	Minute,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fmt {
	Plain,
	Date(Vec<DatePart>),
	Pad(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Filter {
	Lower,
	Trim,
	Ascii,
	Slug,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
	Lit(String),
	Sub { source: Source, fmt: Fmt, filters: Vec<Filter> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Template {
	pub base: Base,
	/// One entry per path component below the base; the last is the name.
	pub segments: Vec<Vec<Piece>>,
}

const DEFAULT_DATE: &str = "%Y-%m-%d";

impl Template {
	pub(crate) fn parse(to: &str, op: ActionOp, form: &FormSchema) -> Result<Self, String> {
		if to.is_empty() || to.chars().count() > MAX_TEMPLATE {
			return Err(format!("a template of 1–{MAX_TEMPLATE} characters"));
		}
		if to.chars().any(|c| c.is_control() || text::is_invisible(c)) {
			return Err("contains a control or invisible character".into());
		}
		if to.contains('\\') {
			return Err("separate folders with '/' (a '\\' is not allowed)".into());
		}
		let raw = split(to)?;
		let mut base = Base::ItemDir;
		let mut rest = &raw[..];
		if let Some(first) = raw.first()
			&& let Some(inner) = first.strip_prefix('{').and_then(|s| s.strip_suffix('}'))
			&& !inner.contains(['{', '}'])
		{
			let dir_field = inner.strip_prefix("form.").filter(
				|f| matches!(form.properties.get(*f), Some(Field::String(s)) if s.format == Some(StringFormat::Dir)),
			);
			if inner == "item.dir" || dir_field.is_some() {
				if op == ActionOp::Rename {
					return Err("a rename target is a name only: no folder".into());
				}
				if let Some(f) = dir_field {
					required(form, f)?;
					base = Base::Form(f.to_owned());
				}
				rest = &raw[1..];
				if rest.is_empty() {
					return Err("needs a name after the folder".into());
				}
			}
		}
		if op == ActionOp::Rename && rest.len() > 1 {
			return Err("a rename target is a name only: no '/'".into());
		}
		let segments = rest.iter().map(|s| pieces(s, form)).collect::<Result<Vec<_>, _>>()?;
		Ok(Self { base, segments })
	}
}

/// The template split at `/` outside braces; empty components refused.
fn split(to: &str) -> Result<Vec<&str>, String> {
	let (mut out, mut start, mut open) = (Vec::new(), 0, false);
	for (i, c) in to.char_indices() {
		match c {
			'{' if open => return Err("'{' inside '{…}'".into()),
			'{' => open = true,
			'}' if !open => return Err("'}' without '{'".into()),
			'}' => open = false,
			'/' if !open => {
				out.push(&to[start..i]);
				start = i + 1;
			}
			_ => {}
		}
	}
	if open {
		return Err("'{' without '}'".into());
	}
	out.push(&to[start..]);
	if out.iter().any(|s| s.is_empty()) {
		return Err("an empty path component (a leading, trailing or doubled '/')".into());
	}
	if out.iter().any(|s| *s == "." || *s == "..") {
		return Err("'.' and '..' are not allowed".into());
	}
	Ok(out)
}

fn pieces(segment: &str, form: &FormSchema) -> Result<Vec<Piece>, String> {
	let mut out = Vec::new();
	let mut rest = segment;
	while !rest.is_empty() {
		match rest.find('{') {
			Some(0) => {
				let end = rest.find('}').ok_or("'{' without '}'")?;
				out.push(substitution(&rest[1..end], form)?);
				rest = &rest[end + 1..];
			}
			Some(i) => {
				out.push(Piece::Lit(rest[..i].to_owned()));
				rest = &rest[i..];
			}
			None => {
				out.push(Piece::Lit(rest.to_owned()));
				rest = "";
			}
		}
	}
	Ok(out)
}

fn substitution(inner: &str, form: &FormSchema) -> Result<Piece, String> {
	let mut parts = inner.split('|');
	let head = parts.next().unwrap_or_default();
	let (name, format) = match head.split_once(':') {
		Some((n, f)) => (n, Some(f)),
		None => (head, None),
	};
	let shown = text::show(name);
	let (source, kind) = match name {
		"item.name" => (Source::Name, Kind::Text),
		"item.stem" => (Source::Stem, Kind::Text),
		"item.ext" => (Source::Ext, Kind::Text),
		"item.mtime" => (Source::Mtime, Kind::Time),
		"today" => (Source::Today, Kind::Date),
		"n" => (Source::N, Kind::Count),
		"item.dir" => return Err("{item.dir} can only start a move or copy target".into()),
		_ => {
			let Some(field) = name.strip_prefix("form.") else {
				return Err(format!(
					"unknown source '{shown}' (item.name, item.stem, item.ext, item.dir, item.mtime, today, n, form.<field>)"
				));
			};
			let kind = match form.properties.get(field) {
				None => return Err(format!("no form field '{}'", text::show(field))),
				Some(Field::String(s)) => match s.format {
					Some(StringFormat::Dir) => {
						return Err(format!("folder field '{field}' can only start a move or copy target"));
					}
					Some(StringFormat::Date) => Kind::Date,
					None => Kind::Text,
				},
				Some(Field::Integer(_)) => Kind::Count,
				Some(Field::Number(_) | Field::Boolean(_)) => Kind::Text,
			};
			required(form, field)?;
			(Source::Form(field.to_owned()), kind)
		}
	};
	let fmt = match (format, kind) {
		(None, Kind::Time | Kind::Date) => date(DEFAULT_DATE, false)?,
		(None, _) => Fmt::Plain,
		(Some(f), Kind::Time) => date(f, true)?,
		(Some(f), Kind::Date) => date(f, false)?,
		(Some(f), Kind::Count) => {
			let width = f.strip_prefix('0').and_then(|w| w.parse::<usize>().ok()).filter(|w| (1..=9).contains(w));
			Fmt::Pad(width.ok_or_else(|| format!("'{}': a number format is :0N (N = 1–9), as in :03", text::show(f)))?)
		}
		(Some(_), Kind::Text) => return Err(format!("'{shown}' takes no format")),
	};
	let filters = parts
		.map(|f| match f {
			"lower" => Ok(Filter::Lower),
			"trim" => Ok(Filter::Trim),
			"ascii" => Ok(Filter::Ascii),
			"slug" => Ok(Filter::Slug),
			_ => Err(format!("unknown filter '{}' (lower, trim, ascii, slug)", text::show(f))),
		})
		.collect::<Result<Vec<_>, _>>()?;
	if filters.len() > 4 {
		return Err("at most 4 filters".into());
	}
	Ok(Piece::Sub { source, fmt, filters })
}

#[derive(Clone, Copy)]
enum Kind {
	Text,
	Time,
	Date,
	Count,
}

fn date(f: &str, time: bool) -> Result<Fmt, String> {
	let mut parts = Vec::new();
	let mut lit = String::new();
	let mut chars = f.chars();
	while let Some(c) = chars.next() {
		if c != '%' {
			lit.push(c);
			continue;
		}
		let part = match chars.next() {
			Some('Y') => DatePart::Year,
			Some('m') => DatePart::Month,
			Some('d') => DatePart::Day,
			Some('H') if time => DatePart::Hour,
			Some('M') if time => DatePart::Minute,
			Some('H' | 'M') => return Err("%H and %M need a time: only {item.mtime} has one".into()),
			_ => return Err(format!("'{}': a date format uses %Y %m %d %H %M", text::show(f))),
		};
		if !lit.is_empty() {
			parts.push(DatePart::Lit(std::mem::take(&mut lit)));
		}
		parts.push(part);
	}
	if !lit.is_empty() {
		parts.push(DatePart::Lit(lit));
	}
	if !parts.iter().any(|p| !matches!(p, DatePart::Lit(_))) {
		return Err(format!("'{}': a date format needs at least one of %Y %m %d %H %M", text::show(f)));
	}
	Ok(Fmt::Date(parts))
}

fn required(form: &FormSchema, field: &str) -> Result<(), String> {
	if form.required.iter().any(|r| r == field) {
		Ok(())
	} else {
		Err(format!("form field '{field}' is used in the template, so it must be listed in form.required"))
	}
}
