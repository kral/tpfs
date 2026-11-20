//! Expansion of a recipe over a selection into a plan.

use std::{
	collections::{BTreeMap, BTreeSet},
	path::{Component, Path, PathBuf},
};

use serde_json::Value;
use tpfs_core::{Op, Plan};

use crate::{
	ActionOp, Field, Kind, OnConflict, Recipe, RecipeError, StringFormat,
	template::{Base, DatePart, Filter, Fmt, Piece, Source, Template},
	text,
};

/// Errors listed one by one before the rest is only counted.
const LISTED_ERRORS: usize = 20;
/// Names tried for one entry with `on_conflict: number` (`_2` … `_1000`).
const MAX_NUMBER: u32 = 1_000;
/// Free-name checks (calls to `exists`) for numbering in one run.
const NUMBER_BUDGET: usize = 50_000;

/// A calendar date and time of day, in the time zone the person sees (the front end converts).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct Stamp {
	pub year: i32,
	pub month: u8,
	pub day: u8,
	pub hour: u8,
	pub minute: u8,
}

impl Stamp {
	/// A valid date and time, or `None`.
	pub fn new(year: i32, month: u8, day: u8, hour: u8, minute: u8) -> Option<Self> {
		let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
		let days = match month {
			1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
			4 | 6 | 9 | 11 => 30,
			2 if leap => 29,
			2 => 28,
			_ => return None,
		};
		let ok = (1..=9999).contains(&year) && (1..=days).contains(&day) && hour < 24 && minute < 60;
		ok.then_some(Self { year, month, day, hour, minute })
	}

	/// A valid date at midnight, or `None`.
	pub fn date(year: i32, month: u8, day: u8) -> Option<Self> {
		Self::new(year, month, day, 0, 0)
	}

	/// `YYYY-MM-DD`, optionally followed by `THH:MM` (or a space instead of `T`; seconds ignored).
	pub fn parse(s: &str) -> Option<Self> {
		let (date, time) = match s.split_once(['T', ' ']) {
			Some((d, t)) => (d, Some(t)),
			None => (s, None),
		};
		let mut d = date.split('-');
		let (y, m, dd) = (d.next()?, d.next()?, d.next()?);
		if d.next().is_some() || y.len() != 4 || m.len() != 2 || dd.len() != 2 {
			return None;
		}
		let (h, mi) = match time {
			None => (0, 0),
			Some(t) => {
				let mut t = t.split(':');
				let (h, mi) = (t.next()?, t.next()?);
				if h.len() != 2 || mi.len() != 2 || t.next().is_some_and(|s| s.len() != 2 || s.parse::<u8>().is_err()) {
					return None;
				}
				(h.parse().ok()?, mi.parse().ok()?)
			}
		};
		let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
		if !(digits(y) && digits(m) && digits(dd)) {
			return None;
		}
		Self::new(y.parse().ok()?, m.parse().ok()?, dd.parse().ok()?, h, mi)
	}

	/// The UTC date and time of a Unix time in seconds (for a front end without a time zone).
	pub fn from_unix(secs: i64) -> Option<Self> {
		let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
		// Howard Hinnant's civil_from_days.
		let z = days + 719_468;
		let era = z.div_euclid(146_097);
		let doe = z - era * 146_097;
		let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
		let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
		let mp = (5 * doy + 2) / 153;
		let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
		let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
		let year = yoe + era * 400 + i64::from(month <= 2);
		Self::new(i32::try_from(year).ok()?, month, day, (rem / 3600) as u8, (rem % 3600 / 60) as u8)
	}

	fn format(&self, parts: &[DatePart]) -> String {
		let mut out = String::new();
		for p in parts {
			match p {
				DatePart::Lit(s) => out.push_str(s),
				DatePart::Year => out.push_str(&format!("{:04}", self.year)),
				DatePart::Month => out.push_str(&format!("{:02}", self.month)),
				DatePart::Day => out.push_str(&format!("{:02}", self.day)),
				DatePart::Hour => out.push_str(&format!("{:02}", self.hour)),
				DatePart::Minute => out.push_str(&format!("{:02}", self.minute)),
			}
		}
		out
	}
}

/// One selected entry, in the order shown to the person.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Item {
	/// Absolute path.
	pub path: PathBuf,
	/// A folder (checked against `input.kind`).
	pub dir: bool,
	/// Last modification, in local time.
	pub mtime: Stamp,
}

impl Item {
	pub fn new(path: impl Into<PathBuf>, dir: bool, mtime: Stamp) -> Self {
		Self { path: path.into(), dir, mtime }
	}
}

/// The filled-in form: field name → JSON value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Values(pub(crate) BTreeMap<String, Value>);

impl Values {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn insert(&mut self, field: impl Into<String>, value: Value) {
		self.0.insert(field.into(), value);
	}

	pub fn get(&self, field: &str) -> Option<&Value> {
		self.0.get(field)
	}

	pub fn contains(&self, field: &str) -> bool {
		self.0.contains_key(field)
	}
}

impl From<BTreeMap<String, Value>> for Values {
	fn from(map: BTreeMap<String, Value>) -> Self {
		Self(map)
	}
}

impl FromIterator<(String, Value)> for Values {
	fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
		Self(iter.into_iter().collect())
	}
}

/// A checked form value, ready for a template.
enum Ready {
	Text(String),
	Int(i64),
	Date(Stamp),
	Dir(PathBuf),
}

impl Recipe {
	/// The scope's folders with `~` replaced by `home` (no disk access).
	pub fn resolve_roots(&self, home: &Path) -> Vec<PathBuf> {
		self.scope
			.roots
			.iter()
			.map(|r| match r.strip_prefix('~') {
				Some("") => home.to_path_buf(),
				Some(rest) => home.join(rest.trim_start_matches('/')),
				None => PathBuf::from(r),
			})
			.collect()
	}
}

/// Expand `recipe` over `items` into a plan: mkdirs for missing folders first (each once), then
/// one move, copy or rename per entry. `roots` is the scope as the person confirmed it; every
/// entry and every target must lie inside it, or the whole run is refused. `exists` answers
/// whether a path is taken on disk (pass `|_| false` to check a recipe without a disk). The same
/// input always gives the same plan, byte for byte.
pub fn expand(
	recipe: &Recipe,
	roots: &[PathBuf],
	items: &[Item],
	values: &Values,
	today: Stamp,
	exists: &dyn Fn(&Path) -> bool,
) -> Result<Plan, Vec<RecipeError>> {
	recipe.check()?;
	let template = Template::parse(&recipe.action.to, recipe.action.op, &recipe.form)
		.map_err(|m| vec![RecipeError::new("action.to", m)])?;
	let mut errors = Errors::default();
	let roots: Vec<PathBuf> = roots
		.iter()
		.enumerate()
		.filter_map(|(i, r)| {
			let n = tpfs_core::normalize(r);
			if n.is_none() {
				errors.push(format!("roots[{i}]"), "must be an absolute folder");
			}
			n
		})
		.collect();
	if roots.is_empty() {
		errors.push("roots", "no folder to work in");
	}
	let ready = check_values(recipe, &roots, values, &mut errors);
	let items = check_items(recipe, &roots, items, &mut errors);
	errors.done()?;

	let ctx = Ctx { template: &template, ready: &ready, today };
	let mut run = Run::new(recipe, &roots, &items, exists);
	for (i, (item, name)) in items.iter().enumerate() {
		run.add(i, item, name, &ctx, &mut errors);
	}
	errors.done()?;
	run.finish()
}

struct Ctx<'a> {
	template: &'a Template,
	ready: &'a BTreeMap<String, Ready>,
	today: Stamp,
}

#[derive(Default)]
struct Errors {
	list: Vec<RecipeError>,
	more: usize,
}

impl Errors {
	fn push(&mut self, path: impl Into<String>, message: impl Into<String>) {
		if self.list.len() < LISTED_ERRORS {
			self.list.push(RecipeError::new(path, message));
		} else {
			self.more += 1;
		}
	}

	fn done(&mut self) -> Result<(), Vec<RecipeError>> {
		if self.list.is_empty() {
			return Ok(());
		}
		let mut list = std::mem::take(&mut self.list);
		if self.more > 0 {
			list.push(RecipeError::new("", format!("… and {} more", self.more)));
		}
		Err(list)
	}
}

fn check_values(recipe: &Recipe, roots: &[PathBuf], values: &Values, errors: &mut Errors) -> BTreeMap<String, Ready> {
	let form = &recipe.form;
	let mut ready = BTreeMap::new();
	for key in values.0.keys().filter(|k| !form.properties.contains_key(*k)) {
		errors.push(format!("values.{}", text::show(key)), "no such field in the form");
	}
	for name in &form.required {
		if !values.0.contains_key(name) {
			errors.push(format!("values.{name}"), "is required");
		}
	}
	for (name, field) in &form.properties {
		let Some(v) = values.0.get(name) else { continue };
		let at = format!("values.{name}");
		let value = match (field, v) {
			(Field::String(f), Value::String(s)) => {
				let len = s.chars().count() as u64;
				if f.min_length.is_some_and(|m| len < m.into()) || f.max_length.is_some_and(|m| len > m.into()) {
					errors.push(&at, "has the wrong length");
					continue;
				}
				if let Some(p) = &f.pattern
					&& !text::regex(p, usize::MAX).is_ok_and(|r| r.is_match(s))
				{
					errors.push(&at, "does not match the field's pattern");
					continue;
				}
				if let Some(c) = &f.choices
					&& !c.contains(s)
				{
					errors.push(&at, "is not one of the field's choices");
					continue;
				}
				match f.format {
					None => Ready::Text(s.clone()),
					Some(StringFormat::Date) => match Stamp::parse(s) {
						Some(d) if s.len() == 10 => Ready::Date(d),
						_ => {
							errors.push(&at, "is not a date (YYYY-MM-DD)");
							continue;
						}
					},
					Some(StringFormat::Dir) => {
						let dir = (!s.chars().any(|c| c.is_control() || text::is_invisible(c)))
							.then(|| tpfs_core::normalize(Path::new(s)))
							.flatten();
						match dir {
							Some(d) if roots.iter().any(|r| inside(&d, r, false)) => Ready::Dir(d),
							_ => {
								errors.push(&at, "must be a folder inside the recipe's scope");
								continue;
							}
						}
					}
				}
			}
			(Field::Integer(f), Value::Number(n)) if n.as_i64().is_some() => {
				let i = n.as_i64().unwrap_or_default();
				if f.minimum.is_some_and(|m| (i as f64) < m) || f.maximum.is_some_and(|m| (i as f64) > m) {
					errors.push(&at, "is out of range");
					continue;
				}
				Ready::Int(i)
			}
			(Field::Number(f), Value::Number(n)) => {
				let x = n.as_f64().unwrap_or(f64::NAN);
				if !x.is_finite() || f.minimum.is_some_and(|m| x < m) || f.maximum.is_some_and(|m| x > m) {
					errors.push(&at, "is out of range");
					continue;
				}
				Ready::Text(n.to_string())
			}
			(Field::Boolean(_), Value::Bool(b)) => Ready::Text(b.to_string()),
			_ => {
				let want = match field {
					Field::String(_) => "a string",
					Field::Integer(_) => "a whole number",
					Field::Number(_) => "a number",
					Field::Boolean(_) => "true or false",
				};
				errors.push(&at, format!("must be {want}"));
				continue;
			}
		};
		ready.insert(name.clone(), value);
	}
	ready
}

/// The selection, normalised, with each entry's name; problems reported for the whole selection.
fn check_items(recipe: &Recipe, roots: &[PathBuf], items: &[Item], errors: &mut Errors) -> Vec<(Item, String)> {
	let input = &recipe.input;
	let count = items.len();
	if count < input.min as usize || count > input.max as usize {
		errors.push("items", format!("takes {}–{} entries, {count} selected", input.min, input.max));
	}
	let mut out = Vec::with_capacity(count);
	for (i, item) in items.iter().enumerate() {
		let at = format!("items[{i}]");
		let Some(path) = tpfs_core::normalize(&item.path) else {
			errors.push(at, "is not an absolute path");
			continue;
		};
		let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
			errors.push(at, "has no name, or a name that is not valid Unicode");
			continue;
		};
		if !roots.iter().any(|r| inside(&path, r, true)) {
			errors.push(at, format!("'{}' is outside the recipe's folders", text::show(&name)));
			continue;
		}
		if item.dir != (input.kind == Kind::Dir) {
			let want = if input.kind == Kind::Dir { "folders" } else { "files" };
			errors.push(at, format!("'{}': this recipe takes {want}", text::show(&name)));
			continue;
		}
		if !input.glob.is_empty() && !input.glob.iter().any(|g| text::glob(g, &name)) {
			errors.push(at, format!("'{}' matches none of {}", text::show(&name), input.glob.join(", ")));
			continue;
		}
		out.push((Item { path, ..item.clone() }, name));
	}
	out
}

/// Whether `path` is `root` or lies inside it, compared as [`expand`] compares them (letter case
/// ignored on Windows and macOS). Both should be normalised the same way (for example canonical).
pub fn within(path: &Path, root: &Path) -> bool {
	inside(path, root, false)
}

/// Whether `path` lies in `root` (strictly below it if `strict`). Letter case is ignored where the
/// usual file systems ignore it: this is a check before the plan's own validation, which confines
/// every step to the roots by itself.
fn inside(path: &Path, root: &Path, strict: bool) -> bool {
	let (mut p, mut r) = (path.components(), root.components());
	loop {
		match (r.next(), p.next()) {
			(None, rest) => return !strict || rest.is_some(),
			(Some(_), None) => return false,
			(Some(a), Some(b)) if same(a, b) => {}
			_ => return false,
		}
	}
}

/// One folder name, as the usual volumes of this system compare it: Windows ignores letter case;
/// macOS ignores it too (the default) and treats composed and decomposed accents as one name;
/// elsewhere names are compared byte for byte. This is a check before the plan's own validation,
/// which confines every step to the roots exactly, so it may be lenient.
fn same(a: Component<'_>, b: Component<'_>) -> bool {
	if a == b {
		return true;
	}
	let (x, y) = (a.as_os_str().to_string_lossy(), b.as_os_str().to_string_lossy());
	if cfg!(windows) {
		x.to_lowercase() == y.to_lowercase()
	} else if cfg!(target_vendor = "apple") {
		tpfs_core::nfd(&x.to_lowercase()) == tpfs_core::nfd(&y.to_lowercase())
	} else {
		false
	}
}

/// How paths are compared for collisions: letter case and accent composition ignored everywhere
/// (a recipe must not depend on the file system it runs on).
fn key(p: &Path) -> String {
	tpfs_core::nfd(&p.to_string_lossy().to_lowercase())
}

struct Run<'a> {
	recipe: &'a Recipe,
	roots: &'a [PathBuf],
	exists: &'a dyn Fn(&Path) -> bool,
	sources: BTreeSet<String>,
	taken: BTreeSet<String>,
	dirs: BTreeSet<String>,
	mkdirs: Vec<Op>,
	ops: Vec<Op>,
	collisions: Vec<String>,
	budget: usize,
}

impl<'a> Run<'a> {
	fn new(
		recipe: &'a Recipe,
		roots: &'a [PathBuf],
		items: &[(Item, String)],
		exists: &'a dyn Fn(&Path) -> bool,
	) -> Self {
		Self {
			recipe,
			roots,
			exists,
			sources: items.iter().map(|(i, _)| key(&i.path)).collect(),
			taken: BTreeSet::new(),
			dirs: BTreeSet::new(),
			mkdirs: Vec::new(),
			ops: Vec::new(),
			collisions: Vec::new(),
			budget: NUMBER_BUDGET,
		}
	}

	fn add(&mut self, i: usize, item: &Item, name: &str, ctx: &Ctx<'_>, errors: &mut Errors) {
		let at = format!("items[{i}]");
		let parent = item.path.parent().unwrap_or(&item.path).to_path_buf();
		let mut parts = Vec::with_capacity(ctx.template.segments.len());
		for segment in &ctx.template.segments {
			let part = component(segment, item, name, i + 1, ctx);
			if let Some(why) = text::bad_component(&part) {
				errors.push(&at, format!("'{}' gives the name '{}', which {why}", text::show(name), text::show(&part)));
				return;
			}
			parts.push(part);
		}
		let base = match &ctx.template.base {
			_ if self.recipe.action.op == ActionOp::Rename => parent.clone(),
			Base::ItemDir => parent.clone(),
			Base::Form(f) => match ctx.ready.get(f) {
				Some(Ready::Dir(d)) => d.clone(),
				_ => return, // reported by check_values
			},
		};
		let Some((last, folders)) = parts.split_last() else { return };
		let mut dir = base;
		for f in folders {
			dir.push(f);
		}
		if !self.roots.iter().any(|r| inside(&dir, r, false)) {
			errors.push(&at, format!("'{}' would go outside the recipe's folders", text::show(name)));
			return;
		}
		let mut target = dir.join(last);
		if target == item.path {
			return; // already where and what it should be
		}
		let own = key(&item.path);
		let free = |run: &Self, t: &Path| {
			let k = key(t);
			k == own
				|| !(run.taken.contains(&k) || run.dirs.contains(&k) || run.sources.contains(&k) || (run.exists)(t))
		};
		if !free(self, &target) {
			match self.recipe.action.on_conflict {
				OnConflict::Stop => {
					self.collisions.push(format!(
						"'{}' → '{}' is taken",
						text::show(name),
						tpfs_core::sanitize(target.as_os_str())
					));
					return;
				}
				OnConflict::Number => {
					let (stem, ext) = text::split_ext(last);
					let mut found = None;
					for k in 2..=MAX_NUMBER {
						if self.budget == 0 {
							break;
						}
						self.budget -= 1;
						let n = numbered(stem, ext, k);
						let t = dir.join(&n);
						if text::bad_component(&n).is_none() && free(self, &t) {
							found = Some(t);
							break;
						}
					}
					let Some(t) = found else {
						errors
							.push(&at, format!("no free name for '{}' (tried up to _{MAX_NUMBER})", text::show(last)));
						return;
					};
					target = t;
				}
			}
		}
		// Folders the template adds below its base, each created once, shallowest first.
		let mut sub = dir.clone();
		let mut needed = Vec::new();
		for _ in folders {
			needed.push(sub.clone());
			sub.pop();
		}
		for d in needed.into_iter().rev() {
			let k = key(&d);
			if self.dirs.contains(&k) || self.sources.contains(&k) || (self.exists)(&d) {
				continue;
			}
			if self.taken.contains(&k) {
				errors.push(&at, format!("folder '{}' is also a target name", tpfs_core::sanitize(d.as_os_str())));
				return;
			}
			self.dirs.insert(k);
			self.mkdirs.push(Op::Mkdir { path: d });
		}
		self.taken.insert(key(&target));
		self.ops.push(match self.recipe.action.op {
			ActionOp::Move => Op::Move { from: item.path.clone(), to: target },
			ActionOp::Copy => Op::Copy { from: item.path.clone(), to: target },
			ActionOp::Rename => Op::Rename {
				path: item.path.clone(),
				new_name: target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
			},
		});
	}

	fn finish(mut self) -> Result<Plan, Vec<RecipeError>> {
		if !self.collisions.is_empty() {
			let mut errors = Errors::default();
			for c in self.collisions.drain(..) {
				errors.push("action.on_conflict", c);
			}
			errors.done()?;
		}
		if self.ops.is_empty() {
			return Err(vec![RecipeError::new("items", "nothing to do: every entry already has its target")]);
		}
		let total = self.mkdirs.len() + self.ops.len();
		if total > tpfs_core::MAX_OPS {
			return Err(vec![RecipeError::new(
				"items",
				format!("{total} steps; a plan has at most {}", tpfs_core::MAX_OPS),
			)]);
		}
		let r = self.recipe;
		let verb = match r.action.op {
			ActionOp::Move => "move",
			ActionOp::Copy => "copy",
			ActionOp::Rename => "rename",
		};
		let n = self.ops.len();
		let noun = match (r.input.kind, n) {
			(Kind::File, 1) => "file",
			(Kind::File, _) => "files",
			(Kind::Dir, 1) => "folder",
			(Kind::Dir, _) => "folders",
		};
		let reason = format!("Recipe \"{}\" v{}: {verb} {n} {noun}", r.title, r.version);
		let mut ops = self.mkdirs;
		ops.append(&mut self.ops);
		Ok(Plan::new(ops, reason))
	}
}

fn numbered(stem: &str, ext: &str, k: u32) -> String {
	if ext.is_empty() { format!("{stem}_{k}") } else { format!("{stem}_{k}.{ext}") }
}

/// One path component of the target for one entry.
fn component(segment: &[Piece], item: &Item, name: &str, n: usize, ctx: &Ctx<'_>) -> String {
	let mut out = String::new();
	for piece in segment {
		match piece {
			Piece::Lit(s) => out.push_str(s),
			Piece::Sub { source, fmt, filters } => {
				let (stem, ext) = text::split_ext(name);
				let mut v = match (source, fmt) {
					(Source::Name, _) => name.to_owned(),
					(Source::Stem, _) => stem.to_owned(),
					(Source::Ext, _) => ext.to_owned(),
					(Source::Mtime, Fmt::Date(p)) => item.mtime.format(p),
					(Source::Today, Fmt::Date(p)) => ctx.today.format(p),
					(Source::N, Fmt::Pad(w)) => format!("{n:0w$}"),
					(Source::N, _) => n.to_string(),
					(Source::Form(f), fmt) => match (ctx.ready.get(f), fmt) {
						(Some(Ready::Text(s)), _) => s.clone(),
						(Some(Ready::Int(i)), Fmt::Pad(w)) => format!("{i:0w$}"),
						(Some(Ready::Int(i)), _) => i.to_string(),
						(Some(Ready::Date(d)), Fmt::Date(p)) => d.format(p),
						_ => String::new(),
					},
					_ => String::new(),
				};
				for f in filters {
					v = match f {
						Filter::Lower => text::lower(&v),
						Filter::Trim => v.trim().to_owned(),
						Filter::Ascii => text::ascii(&v),
						Filter::Slug => text::slug(&v),
					};
				}
				// A value never adds a folder.
				out.push_str(&v.replace(['/', '\\'], "_"));
			}
		}
	}
	out
}
