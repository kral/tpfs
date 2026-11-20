//! `tpfs recipes` and `tpfs recipe …`: review, install and run recipes that agents proposed.

use std::{
	fs, io,
	io::IsTerminal,
	path::{Path, PathBuf},
	process::ExitCode,
	time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use tpfs_core::{Source, Validator, sanitize};
use tpfs_mcp::{InstalledRecipes, PendingRecipe, RecipeStore};
use tpfs_recipe::{Field, Recipe, Stamp, StringFormat};

use super::{Fail, Res, ask, cancelled, execute, open_state, present, rejected, state_dir, usage};

/// Runs during which a recipe is new: the run needs a typed `yes`.
const NEW_RUNS: u32 = 3;

fn stores(args: &super::Args) -> Result<(RecipeStore, InstalledRecipes), Fail> {
	let state = state_dir(args)?;
	Ok((RecipeStore::new(&state), InstalledRecipes::new(&state)))
}

fn s(text: &str) -> String {
	sanitize(text.as_ref())
}

/// `tpfs recipes`: proposals waiting for review, then installed recipes.
pub(super) fn list(args: &super::Args) -> Res {
	let (inbox, shelf) = stores(args)?;
	let (waiting, damaged) = inbox.scan().map_err(|e| e.to_string())?;
	let (installed, broken) = shelf.list().map_err(|e| e.to_string())?;
	if waiting.is_empty() && damaged.is_empty() && installed.is_empty() && broken.is_empty() {
		println!("No recipes. An AI agent can propose one through `tpfs mcp` (recipe_propose).");
		return Ok(ExitCode::SUCCESS);
	}
	if !waiting.is_empty() || !damaged.is_empty() {
		println!("Proposed by agents (review with `tpfs recipe install <id>`, or `tpfs recipe reject <id>`):");
		for p in &waiting {
			let note = if p.note.is_empty() { String::new() } else { format!(" — {}", s(&p.note)) };
			println!(
				"  {}  \"{}\" v{} ({}) from {}{note}",
				p.id,
				s(&p.recipe.title),
				p.recipe.version,
				p.recipe.id,
				s(&p.client)
			);
		}
		for id in &damaged {
			println!("  {id}  (unreadable or changed; `tpfs recipe reject {id}` drops it)");
		}
	}
	if !installed.is_empty() || !broken.is_empty() {
		println!("Installed (run with `tpfs recipe run <recipe> <entry>…`):");
		for i in &installed {
			let fields: Vec<&str> = i.recipe.form.properties.keys().map(String::as_str).collect();
			let fields = if fields.is_empty() { String::new() } else { format!(", --set {}", fields.join(" --set ")) };
			println!(
				"  {}  v{}  \"{}\"  ({}{fields})",
				i.recipe.id,
				i.recipe.version,
				s(&i.recipe.title),
				op_word(i.recipe.action.op)
			);
		}
		for id in &broken {
			println!("  {id}  DISABLED: changed outside tpfs; `tpfs recipe remove {id}`, then install a new proposal");
		}
	}
	Ok(ExitCode::SUCCESS)
}

/// `tpfs recipe <what> …`.
pub(super) fn run(args: &super::Args) -> Res {
	let what = args.plan.as_ref().and_then(|p| p.to_str()).unwrap_or_default();
	let id = || args.extra.as_ref().and_then(|p| p.to_str()).map(str::to_owned).ok_or_else(|| usage("missing <id>"));
	let one = |n: usize| {
		if args.rest.len() > n {
			Err(usage(format!("recipe {what}: too many arguments (see `tpfs recipe --help`)")))
		} else {
			Ok(())
		}
	};
	if what != "install" && !args.roots.is_empty() {
		return Err(usage("--root applies to `tpfs recipe install` only"));
	}
	if what != "run" && (args.approve.is_some() || !args.sets.is_empty()) {
		return Err(usage("--set and --approve apply to `tpfs recipe run` only"));
	}
	match what {
		"install" => {
			one(0)?;
			install(args, &id()?, None)
		}
		"reject" => {
			one(0)?;
			let id = id()?;
			let (inbox, _) = stores(args)?;
			inbox.reject(&id).map_err(|e| missing(e, "tpfs recipes"))?;
			println!("Rejected {}. The agent sees this in recipe_status.", s(&id));
			Ok(ExitCode::SUCCESS)
		}
		"show" => {
			one(0)?;
			let id = id()?;
			let (inbox, shelf) = stores(args)?;
			match shelf.get(&id) {
				Ok(i) => {
					describe(&i.recipe);
					println!("Folders (as confirmed): {}", shown(&i.roots));
					println!("Runs so far: {}", i.runs);
				}
				Err(e) if e.kind() == io::ErrorKind::NotFound => {
					let p = inbox.get(&id).map_err(|e| missing(e, "tpfs recipes"))?;
					describe_proposal(&p);
				}
				Err(e) => return Err(e.to_string().into()),
			}
			Ok(ExitCode::SUCCESS)
		}
		"remove" => {
			one(0)?;
			let id = id()?;
			stores(args)?.1.remove(&id).map_err(|e| missing(e, "tpfs recipes"))?;
			println!("Removed recipe {}.", s(&id));
			Ok(ExitCode::SUCCESS)
		}
		"run" => run_recipe(args, &id()?),
		"" => Err(usage("missing what to do: install, reject, show, run or remove (see `tpfs recipe --help`)")),
		other => Err(usage(format!("unknown recipe command '{}' (install, reject, show, run, remove)", s(other)))),
	}
}

fn op_word(op: tpfs_recipe::ActionOp) -> &'static str {
	match op {
		tpfs_recipe::ActionOp::Move => "move",
		tpfs_recipe::ActionOp::Copy => "copy",
		tpfs_recipe::ActionOp::Rename => "rename",
	}
}

fn missing(e: io::Error, list: &str) -> Fail {
	match e.kind() {
		io::ErrorKind::NotFound | io::ErrorKind::InvalidInput => usage(format!("{e} (see `{list}`)")),
		_ => e.to_string().into(),
	}
}

fn shown(roots: &[PathBuf]) -> String {
	roots.iter().map(|r| sanitize(r.as_os_str())).collect::<Vec<_>>().join(", ")
}

fn home() -> Result<PathBuf, Fail> {
	std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
		.map(PathBuf::from)
		.ok_or_else(|| "cannot tell the home folder (HOME or USERPROFILE)".into())
}

pub(super) fn describe_proposal(p: &PendingRecipe) {
	println!("Recipe proposal {} from {} (as the agent's client calls itself):", p.id, s(&p.client));
	if !p.note.is_empty() {
		println!("  Note: \"{}\"", s(&p.note));
	}
	describe(&p.recipe);
}

/// Everything a recipe says, as the person must see it before installing it.
fn describe(r: &Recipe) {
	println!("  Title: \"{}\" — {} version {}", s(&r.title), r.id, r.version);
	if let Some(d) = &r.description {
		println!("  Description: \"{}\"", s(d));
	}
	println!("  Folders it may touch: {}", r.scope.roots.iter().map(|x| s(x)).collect::<Vec<_>>().join(", "));
	let kind = match r.input.kind {
		tpfs_recipe::Kind::File => "files",
		tpfs_recipe::Kind::Dir => "folders",
	};
	let glob = if r.input.glob.is_empty() { String::new() } else { format!(" named {}", s(&r.input.glob.join(", "))) };
	println!("  Takes: {}–{} {kind}{glob}", r.input.min, r.input.max);
	if r.form.properties.is_empty() {
		println!("  Form: none");
	}
	for (name, field) in &r.form.properties {
		let required = if r.form.required.contains(name) { "" } else { ", optional" };
		println!("  Field --set {name}=…: {}{required}", field_text(field));
	}
	let op = match r.action.op {
		tpfs_recipe::ActionOp::Move => "move each to",
		tpfs_recipe::ActionOp::Copy => "copy each to",
		tpfs_recipe::ActionOp::Rename => "rename each to",
	};
	let conflict = match r.action.on_conflict {
		tpfs_recipe::OnConflict::Number => "a taken name gets _2, _3 …",
		tpfs_recipe::OnConflict::Stop => "a taken name stops the run",
	};
	println!("  Action: {op} {} ({conflict})", s(&r.action.to));
	println!("  Hash: {}", r.hash());
}

fn field_text(f: &Field) -> String {
	let mut out = Vec::new();
	let (title, kind) = match f {
		Field::String(x) => (
			&x.title,
			match x.format {
				Some(StringFormat::Dir) => "a folder inside the scope",
				Some(StringFormat::Date) => "a date YYYY-MM-DD",
				None => "text",
			},
		),
		Field::Integer(x) => (&x.title, "a whole number"),
		Field::Number(x) => (&x.title, "a number"),
		Field::Boolean(x) => (&x.title, "yes or no"),
	};
	if let Some(t) = title {
		out.push(format!("\"{}\"", s(t)));
	}
	out.push(kind.to_owned());
	match f {
		Field::String(x) => {
			if x.min_length.is_some() || x.max_length.is_some() {
				out.push(format!(
					"{}–{} characters",
					x.min_length.unwrap_or(0),
					x.max_length.map_or("…".into(), |m| m.to_string())
				));
			}
			if let Some(p) = &x.pattern {
				out.push(format!("matching {}", s(p)));
			}
			if let Some(c) = &x.choices {
				out.push(format!("one of {}", s(&c.join(", "))));
			}
		}
		Field::Integer(x) | Field::Number(x) => {
			if x.minimum.is_some() || x.maximum.is_some() {
				out.push(format!(
					"from {} to {}",
					x.minimum.map_or("…".into(), |m| m.to_string()),
					x.maximum.map_or("…".into(), |m| m.to_string())
				));
			}
		}
		Field::Boolean(_) => {}
	}
	out.join(", ")
}

/// Review a proposal and install it (asked in this terminal). `--root` narrows the folders.
/// Returns without asking when `answer` is given (from `tpfs watch`).
pub(super) fn install(args: &super::Args, id: &str, answer: Option<&str>) -> Res {
	let (inbox, shelf) = stores(args)?;
	let p = inbox.get(id).map_err(|e| missing(e, "tpfs recipes"))?;
	if answer.is_none() {
		describe_proposal(&p);
	}
	let scope = p.recipe.resolve_roots(&home()?);
	let wanted = if args.roots.is_empty() { scope.clone() } else { args.roots.clone() };
	let mut roots = Vec::new();
	for r in &wanted {
		let Some(n) = std::path::absolute(r).ok().and_then(|a| tpfs_core::normalize(&a)) else {
			return Err(usage(format!("--root {}: not a folder path", sanitize(r.as_os_str()))));
		};
		if !scope.iter().any(|sc| tpfs_core::normalize(sc).is_some_and(|sc| n.starts_with(&sc))) {
			return Err(usage(format!(
				"--root {} is outside the recipe's folders ({})",
				sanitize(n.as_os_str()),
				shown(&scope)
			)));
		}
		roots.push(n);
	}
	// The folders must exist now: they are what every run is confined to.
	let validator = Validator::new(&roots).map_err(|e| {
		Fail(1, format!("{e}: {}. Create the folder, or install with --root for the ones you want", shown(&roots)))
	})?;
	let roots = validator.roots().to_vec();
	println!("  It will work in: {}", shown(&roots));
	let yes = match answer {
		Some(a) => a == "yes",
		None => {
			if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
				return Err(cancelled("installing a recipe is asked interactively in a terminal"));
			}
			ask("Type 'yes' to install this recipe (anything else leaves it waiting): ")? == "yes"
		}
	};
	if !yes {
		return Err(cancelled("not installed; it stays waiting"));
	}
	shelf.install(&p.recipe, &p.hash, &roots).map_err(|e| e.to_string())?;
	inbox.mark_installed(&p.id, &p.hash).map_err(|e| e.to_string())?;
	println!("Installed {}. Run it with `tpfs recipe run {} <entry>…`.", p.recipe.id, p.recipe.id);
	Ok(ExitCode::SUCCESS)
}

/// `tpfs recipe run <recipe> <entry>… [--set field=value]…`: expand over the entries, show the
/// plan, confirm and execute it like `tpfs apply`.
fn run_recipe(args: &super::Args, id: &str) -> Res {
	let (_, shelf) = stores(args)?;
	let installed = shelf.get(id).map_err(|e| missing(e, "tpfs recipes"))?;
	let recipe = &installed.recipe;
	if args.rest.is_empty() {
		return Err(usage("missing the entries to run it on: tpfs recipe run <recipe> <entry>…"));
	}
	let today = local(SystemTime::now()).ok_or("cannot tell today's date")?;
	let mut items = Vec::new();
	for e in &args.rest {
		let path = std::path::absolute(e)
			.ok()
			.and_then(|a| tpfs_core::normalize(&a))
			.ok_or_else(|| usage(format!("{}: not a path", sanitize(e.as_os_str()))))?;
		let meta =
			fs::symlink_metadata(&path).map_err(|err| Fail(1, format!("{}: {err}", sanitize(path.as_os_str()))))?;
		let mtime = meta.modified().ok().and_then(local).unwrap_or(today);
		items.push(tpfs_recipe::Item { path, dir: meta.is_dir(), mtime });
	}
	let values = values(recipe, &args.sets)?;
	let exists = |p: &Path| fs::symlink_metadata(p).is_ok();
	let plan = tpfs_recipe::expand(recipe, &installed.roots, &items, &values, today, &exists).map_err(|errors| {
		let lines: Vec<String> = errors.iter().map(|e| format!("  {}", s(&e.to_string()))).collect();
		Fail(3, format!("the recipe cannot run on this selection; nothing was changed:\n{}", lines.join("\n")))
	})?;
	let validator = Validator::new(&installed.roots)
		.map_err(|e| Fail(1, format!("{e}: {}", shown(&installed.roots))))?
		.with_protected([state_dir(args)?]);
	let Some((digest, level)) = present(&validator, &plan, Source::Human) else { return Ok(rejected(args)) };
	drop(open_state(args)?);
	if installed.runs < NEW_RUNS && args.approve.is_none() {
		// A new recipe: the person reads its first plans and types the answer out.
		println!("This recipe is new to you (run {} of its first {NEW_RUNS}): check every step.", installed.runs + 1);
		if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
			return Err(cancelled("not interactive: pass --approve <digest> to run exactly this plan"));
		}
		if ask("Type 'yes' to run it: ")? != "yes" {
			return Err(cancelled("cancelled; nothing was changed"));
		}
	} else {
		super::confirm(args, &digest, level)?;
	}
	let code = execute(args, &validator, &plan, open_state(args)?)?;
	if code == ExitCode::SUCCESS {
		shelf.record_run(id).ok();
	}
	Ok(code)
}

/// The form values from `--set field=value`, typed by the form; missing required fields are
/// asked for in a terminal.
fn values(recipe: &Recipe, sets: &[String]) -> Result<tpfs_recipe::Values, Fail> {
	let mut out = tpfs_recipe::Values::default();
	let form = &recipe.form;
	for set in sets {
		let (name, raw) = set.split_once('=').ok_or_else(|| usage(format!("--set {}: use field=value", s(set))))?;
		let field = form.properties.get(name).ok_or_else(|| {
			let names: Vec<&str> = form.properties.keys().map(String::as_str).collect();
			usage(format!("--set: no field '{}' (fields: {})", s(name), names.join(", ")))
		})?;
		out.0.insert(name.to_owned(), typed(name, field, raw)?);
	}
	for name in &form.required {
		if out.0.contains_key(name) {
			continue;
		}
		let field = &form.properties[name];
		if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
			return Err(usage(format!("missing --set {name}=… ({})", field_text(field))));
		}
		let raw = ask(&format!("{name} ({}): ", field_text(field)))?;
		out.0.insert(name.clone(), typed(name, field, &raw)?);
	}
	Ok(out)
}

fn typed(name: &str, field: &Field, raw: &str) -> Result<Value, Fail> {
	let bad = |want: &str| usage(format!("--set {name}: '{}' is not {want}", s(raw)));
	Ok(match field {
		Field::String(f) if f.format == Some(StringFormat::Dir) => {
			let dir = std::path::absolute(raw).map_err(|_| bad("a folder"))?;
			Value::String(dir.to_str().ok_or_else(|| bad("a folder"))?.to_owned())
		}
		Field::String(_) => Value::String(raw.to_owned()),
		Field::Integer(_) => Value::from(raw.trim().parse::<i64>().map_err(|_| bad("a whole number"))?),
		Field::Number(_) => {
			let x: f64 = raw.trim().parse().map_err(|_| bad("a number"))?;
			serde_json::Number::from_f64(x).map(Value::Number).ok_or_else(|| bad("a number"))?
		}
		Field::Boolean(_) => match raw.trim() {
			"yes" | "true" | "y" | "1" => Value::Bool(true),
			"no" | "false" | "n" | "0" => Value::Bool(false),
			_ => return Err(bad("yes or no")),
		},
	})
}

/// A moment as the calendar date and time the person sees, in the system's time zone.
pub(super) fn local(t: SystemTime) -> Option<Stamp> {
	let secs = match t.duration_since(UNIX_EPOCH) {
		Ok(d) => i64::try_from(d.as_secs()).ok()?,
		Err(e) => -i64::try_from(e.duration().as_secs()).ok()?,
	};
	local_secs(secs)
}

#[cfg(unix)]
fn local_secs(secs: i64) -> Option<Stamp> {
	#[allow(clippy::useless_conversion, reason = "time_t is 32 bits on some systems")]
	let t: libc::time_t = secs.try_into().ok()?;
	// SAFETY: `tm` is plain data that localtime_r fills in; both pointers are valid for the call.
	let tm = unsafe {
		let mut tm: libc::tm = std::mem::zeroed();
		if libc::localtime_r(&t, &mut tm).is_null() {
			return None;
		}
		tm
	};
	Stamp::new(
		tm.tm_year + 1900,
		u8::try_from(tm.tm_mon + 1).ok()?,
		u8::try_from(tm.tm_mday).ok()?,
		u8::try_from(tm.tm_hour).ok()?,
		u8::try_from(tm.tm_min).ok()?,
	)
}

#[cfg(windows)]
fn local_secs(secs: i64) -> Option<Stamp> {
	use windows_sys::Win32::{
		Foundation::{FILETIME, SYSTEMTIME},
		System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime},
	};
	let ticks = u64::try_from(secs.checked_mul(10_000_000)?.checked_add(116_444_736_000_000_000)?).ok()?;
	let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
	// SAFETY: plain data structures, valid for the calls; a null time zone means the current one.
	unsafe {
		let mut utc: SYSTEMTIME = std::mem::zeroed();
		let mut here: SYSTEMTIME = std::mem::zeroed();
		if FileTimeToSystemTime(&ft, &mut utc) == 0
			|| SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut here) == 0
		{
			return None;
		}
		Stamp::new(i32::from(here.wYear), here.wMonth as u8, here.wDay as u8, here.wHour as u8, here.wMinute as u8)
	}
}

#[cfg(not(any(unix, windows)))]
fn local_secs(secs: i64) -> Option<Stamp> {
	Stamp::from_unix(secs)
}
