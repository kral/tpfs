//! # tpfs-recipe
//!
//! A **recipe** is a small declarative command: a form (a flat JSON Schema object, the MCP
//! elicitation subset) and an operation template. An AI agent proposes it (`tpfs mcp`); a person
//! installs it in a front end and runs it on their own selection. Each run [expands](expand) the
//! template into an ordinary [`tpfs_core::Plan`], which is then reviewed, journaled and undoable
//! like any other.
//!
//! A recipe is data only: no code, no expressions. This crate does no I/O; the one question it asks
//! about the disk ("does this path exist?") is a callback.

mod ascii;
mod expand;
mod template;
mod text;

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub use expand::{Item, Stamp, Values, expand};

/// The format this crate reads and writes.
pub const FORMAT: &str = "tpfs-recipe/1";
/// Most entries a recipe may take in one run.
pub const MAX_ITEMS: u32 = 5_000;
/// Longest recipe accepted, in bytes of JSON.
pub const MAX_JSON: usize = 64 * 1024;
const MAX_FIELDS: usize = 20;
const MAX_TITLE: usize = 60;
const MAX_DESCRIPTION: usize = 300;
const MAX_TEMPLATE: usize = 500;
const MAX_PATTERN: usize = 200;

/// A recipe, as proposed and stored. Unknown keys are refused everywhere.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
	/// Always [`FORMAT`].
	pub format: String,
	/// `[a-z0-9-]{3,48}`.
	pub id: String,
	/// Grows with every update of the same id (from 1).
	pub version: u32,
	/// At most 60 characters of plain text.
	pub title: String,
	/// At most 300 characters of plain text.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub description: Option<String>,
	pub scope: Scope,
	pub input: Input,
	pub form: FormSchema,
	pub action: Action,
}

/// The folders a recipe may touch, as the author proposes them; the front end confirms (and may
/// narrow) them at install and passes the result to [`expand`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
	/// 1–4 absolute folders; `~` or `~/…` means the user's home.
	pub roots: Vec<String>,
}

/// What a run takes: entries selected by the person.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
	pub kind: Kind,
	/// Name patterns (`*` and `?`, letter case ignored); an entry must match one. Empty: any name.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub glob: Vec<String>,
	/// Fewest entries (default 1).
	#[serde(default = "one")]
	pub min: u32,
	/// Most entries (default and at most [`MAX_ITEMS`]).
	#[serde(default = "max_items")]
	pub max: u32,
}

fn one() -> u32 {
	1
}

fn max_items() -> u32 {
	MAX_ITEMS
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
	File,
	Dir,
}

/// A flat JSON Schema object. Nested objects, arrays and conditional keywords are refused.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormSchema {
	#[serde(rename = "type")]
	pub kind: ObjectType,
	#[serde(default)]
	pub properties: BTreeMap<String, Field>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub required: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectType {
	#[serde(rename = "object")]
	Object,
}

/// One form field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Field {
	String(StringField),
	Integer(NumberField),
	Number(NumberField),
	Boolean(BoolField),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StringField {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub description: Option<String>,
	#[serde(default, rename = "minLength", skip_serializing_if = "Option::is_none")]
	pub min_length: Option<u32>,
	#[serde(default, rename = "maxLength", skip_serializing_if = "Option::is_none")]
	pub max_length: Option<u32>,
	/// A regular expression (crate `regex`: linear time), matched anywhere in the value.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pattern: Option<String>,
	#[serde(default, rename = "enum", skip_serializing_if = "Option::is_none")]
	pub choices: Option<Vec<String>>,
	#[serde(default, rename = "enumNames", skip_serializing_if = "Option::is_none")]
	pub choice_names: Option<Vec<String>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub format: Option<StringFormat>,
	/// A hint for the front end (for example `"otherPanel"`); tpfs ignores it.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub default: Option<Value>,
	#[serde(default, rename = "x-tpfs", skip_serializing_if = "Option::is_none")]
	pub hints: Option<Hints>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StringFormat {
	/// `YYYY-MM-DD`.
	#[serde(rename = "date")]
	Date,
	/// A folder inside the scope; may start a move or copy target.
	#[serde(rename = "x-tpfs-dir")]
	Dir,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NumberField {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub description: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub minimum: Option<f64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub maximum: Option<f64>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub default: Option<Value>,
	#[serde(default, rename = "x-tpfs", skip_serializing_if = "Option::is_none")]
	pub hints: Option<Hints>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoolField {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub description: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub default: Option<Value>,
	#[serde(default, rename = "x-tpfs", skip_serializing_if = "Option::is_none")]
	pub hints: Option<Hints>,
}

/// Hints for the front end; tpfs ignores them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hints {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub remember: Option<bool>,
}

/// What a run does with each entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
	pub op: ActionOp,
	/// The target template: a full path for `move` and `copy`, a name for `rename`.
	pub to: String,
	#[serde(default)]
	pub on_conflict: OnConflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionOp {
	Move,
	Copy,
	Rename,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnConflict {
	/// Append `_2`, `_3` … before the extension.
	Number,
	/// Refuse the run and list the collisions.
	#[default]
	Stop,
}

/// A problem with a recipe, its form values or a run, with the key it concerns
/// (`action.to`, `form.properties.year.maximum`, `items[3]` …).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecipeError {
	pub path: String,
	pub message: String,
}

impl RecipeError {
	pub(crate) fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
		Self { path: path.into(), message: message.into() }
	}
}

impl fmt::Display for RecipeError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		if self.path.is_empty() { f.write_str(&self.message) } else { write!(f, "{}: {}", self.path, self.message) }
	}
}

impl std::error::Error for RecipeError {}

/// Read and check a recipe. A structural error (an unknown key, a wrong type) stops the reading
/// and is the only one reported; otherwise every problem is reported, each with its key.
pub fn parse(json: &str) -> Result<Recipe, Vec<RecipeError>> {
	if json.len() > MAX_JSON {
		return Err(vec![RecipeError::new("", format!("a recipe is at most {MAX_JSON} bytes"))]);
	}
	let mut de = serde_json::Deserializer::from_str(json);
	let recipe: Recipe = serde_path_to_error::deserialize(&mut de).map_err(structural)?;
	de.end().map_err(|e| vec![RecipeError::new("", e.to_string())])?;
	recipe.check()?;
	Ok(recipe)
}

/// [`parse`] for a recipe that is already JSON (as an MCP tool argument is).
pub fn from_value(value: Value) -> Result<Recipe, Vec<RecipeError>> {
	if serde_json::to_vec(&value).map_or(true, |b| b.len() > MAX_JSON) {
		return Err(vec![RecipeError::new("", format!("a recipe is at most {MAX_JSON} bytes"))]);
	}
	let recipe: Recipe = serde_path_to_error::deserialize(value).map_err(structural)?;
	recipe.check()?;
	Ok(recipe)
}

fn structural<E: fmt::Display>(e: serde_path_to_error::Error<E>) -> Vec<RecipeError> {
	let path = e.path().to_string();
	let path = if path == "." { String::new() } else { path };
	vec![RecipeError::new(path, e.inner().to_string())]
}

impl Recipe {
	/// Every rule a recipe must meet beyond its shape, all problems at once.
	pub fn check(&self) -> Result<(), Vec<RecipeError>> {
		let mut errors = Vec::new();
		let mut err = |path: &str, msg: String| errors.push(RecipeError::new(path, msg));
		if self.format != FORMAT {
			err("format", format!("unknown format '{}' (expected '{FORMAT}')", text::show(&self.format)));
		}
		let id_ok = (3..=48).contains(&self.id.len())
			&& self.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
		if !id_ok {
			err("id", "must be 3–48 characters of a–z, 0–9 and '-'".into());
		}
		if self.version == 0 {
			err("version", "starts at 1".into());
		}
		if let Some(m) = text::plain(&self.title, MAX_TITLE, false) {
			err("title", m);
		}
		if let Some(m) = self.description.as_deref().and_then(|d| text::plain(d, MAX_DESCRIPTION, true)) {
			err("description", m);
		}
		if !(1..=4).contains(&self.scope.roots.len()) {
			err("scope.roots", "lists 1–4 folders".into());
		}
		for (i, root) in self.scope.roots.iter().enumerate() {
			if let Some(m) = text::root(root) {
				err(&format!("scope.roots[{i}]"), m);
			}
		}
		let input = &self.input;
		if input.glob.len() > 20 {
			err("input.glob", "at most 20 patterns".into());
		}
		for (i, g) in input.glob.iter().enumerate() {
			if g.is_empty() || g.chars().count() > 64 || g.contains(['/', '\\']) || g.chars().any(char::is_control) {
				err(&format!("input.glob[{i}]"), "a name pattern of 1–64 characters, without '/' or '\\'".into());
			}
		}
		if input.min == 0 || input.max > MAX_ITEMS || input.min > input.max {
			err("input", format!("needs 1 ≤ min ≤ max ≤ {MAX_ITEMS}"));
		}
		self.check_form(&mut err);
		match template::Template::parse(&self.action.to, self.action.op, &self.form) {
			Ok(_) => {}
			Err(m) => err("action.to", m),
		}
		if errors.is_empty() { Ok(()) } else { Err(errors) }
	}

	fn check_form(&self, err: &mut impl FnMut(&str, String)) {
		let form = &self.form;
		if form.properties.len() > MAX_FIELDS {
			err("form.properties", format!("at most {MAX_FIELDS} fields"));
		}
		for (name, field) in &form.properties {
			let at = format!("form.properties.{name}");
			let mut chars = name.chars();
			let name_ok = chars.next().is_some_and(|c| c.is_ascii_lowercase())
				&& chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
				&& name.len() <= 32;
			if !name_ok {
				err(&at, "a field name is a–z followed by up to 31 of a–z, A–Z, 0–9 and '_'".into());
			}
			let (title, description) = match field {
				Field::String(f) => (&f.title, &f.description),
				Field::Integer(f) | Field::Number(f) => (&f.title, &f.description),
				Field::Boolean(f) => (&f.title, &f.description),
			};
			if let Some(m) = title.as_deref().and_then(|t| text::plain(t, MAX_TITLE, false)) {
				err(&format!("{at}.title"), m);
			}
			if let Some(m) = description.as_deref().and_then(|d| text::plain(d, MAX_DESCRIPTION, true)) {
				err(&format!("{at}.description"), m);
			}
			match field {
				Field::String(f) => check_string(f, &at, err),
				Field::Integer(f) | Field::Number(f) => {
					let integer = matches!(field, Field::Integer(_));
					for (key, v) in [("minimum", f.minimum), ("maximum", f.maximum)] {
						if v.is_some_and(|v| !v.is_finite() || (integer && v.fract() != 0.0)) {
							err(&format!("{at}.{key}"), "must be a finite number (whole for an integer)".into());
						}
					}
					if let (Some(a), Some(b)) = (f.minimum, f.maximum)
						&& a > b
					{
						err(&at, "minimum is above maximum".into());
					}
				}
				Field::Boolean(_) => {}
			}
		}
		let mut seen = Vec::new();
		for (i, name) in form.required.iter().enumerate() {
			if !form.properties.contains_key(name) {
				err(&format!("form.required[{i}]"), format!("no field '{}'", text::show(name)));
			} else if seen.contains(&name) {
				err(&format!("form.required[{i}]"), format!("'{name}' is listed twice"));
			}
			seen.push(name);
		}
	}

	/// SHA-256 of the canonical JSON form: what a front end shows and records at install.
	pub fn hash(&self) -> String {
		let bytes = serde_json::to_vec(self).unwrap_or_default();
		Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
	}

	/// JSON Schema of the recipe format.
	pub fn schema() -> Value {
		serde_json::from_str(include_str!("recipe.schema.json")).unwrap_or(Value::Null)
	}
}

fn check_string(f: &StringField, at: &str, err: &mut impl FnMut(&str, String)) {
	if let (Some(a), Some(b)) = (f.min_length, f.max_length)
		&& a > b
	{
		err(at, "minLength is above maxLength".into());
	}
	if f.max_length.is_some_and(|m| m > 1000) {
		err(&format!("{at}.maxLength"), "at most 1000".into());
	}
	if let Some(p) = &f.pattern
		&& let Err(m) = text::regex(p, MAX_PATTERN)
	{
		err(&format!("{at}.pattern"), m);
	}
	match (&f.choices, &f.choice_names) {
		(Some(c), _) if c.is_empty() || c.len() > 50 => err(&format!("{at}.enum"), "lists 1–50 values".into()),
		(Some(c), Some(n)) if c.len() != n.len() => {
			err(&format!("{at}.enumNames"), "needs one name per enum value".into());
		}
		(None, Some(_)) => err(&format!("{at}.enumNames"), "needs an enum".into()),
		_ => {}
	}
	for (key, list) in [("enum", &f.choices), ("enumNames", &f.choice_names)] {
		for (i, v) in list.iter().flatten().enumerate() {
			if let Some(m) = text::plain(v, 200, false) {
				err(&format!("{at}.{key}[{i}]"), m);
			}
		}
	}
	if f.format.is_some() && (f.choices.is_some() || f.pattern.is_some()) {
		err(&format!("{at}.format"), "a date or folder field takes no enum or pattern".into());
	}
}

#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod tests_windows;
