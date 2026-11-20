use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tpfs_core::Op;

use super::*;

fn base(to: &str) -> Value {
	json!({
		"format": FORMAT, "id": "test-recipe", "version": 2, "title": "Test",
		"scope": { "roots": ["/data"] },
		"input": { "kind": "file" },
		"form": { "type": "object", "properties": {
			"label": { "type": "string" },
			"count": { "type": "integer", "minimum": 0, "maximum": 999 },
			"when": { "type": "string", "format": "date" },
			"target": { "type": "string", "format": "x-tpfs-dir" }
		}, "required": ["label", "count", "when", "target"] },
		"action": { "op": "move", "to": to }
	})
}

fn recipe(v: Value) -> Recipe {
	from_value(v).unwrap_or_else(|e| panic!("{e:?}"))
}

fn errors(v: Value) -> Vec<RecipeError> {
	from_value(v).expect_err("accepted")
}

fn values(label: &str) -> Values {
	Values(
		[
			("label".to_owned(), json!(label)),
			("count".to_owned(), json!(7)),
			("when".to_owned(), json!("2026-03-04")),
			("target".to_owned(), json!("/data/out")),
		]
		.into(),
	)
}

fn item(path: &str) -> Item {
	Item { path: path.into(), dir: false, mtime: Stamp::new(2025, 12, 31, 23, 5).unwrap() }
}

fn today() -> Stamp {
	Stamp::date(2026, 11, 20).unwrap()
}

fn run(r: &Recipe, items: &[Item], v: &Values) -> Result<tpfs_core::Plan, Vec<RecipeError>> {
	expand(r, &[PathBuf::from("/data")], items, v, today(), &|_| false)
}

fn targets(plan: &tpfs_core::Plan) -> Vec<String> {
	plan.ops
		.iter()
		.filter_map(|op| match op {
			Op::Move { to, .. } | Op::Copy { to, .. } => Some(to.to_string_lossy().into_owned()),
			Op::Rename { new_name, .. } => Some(new_name.clone()),
			_ => None,
		})
		.collect()
}

fn mkdirs(plan: &tpfs_core::Plan) -> Vec<String> {
	plan.ops
		.iter()
		.filter_map(|op| match op {
			Op::Mkdir { path } => Some(path.to_string_lossy().into_owned()),
			_ => None,
		})
		.collect()
}

fn one(to: &str, name: &str) -> Result<String, Vec<RecipeError>> {
	let r = recipe(base(to));
	run(&r, &[item(&format!("/data/in/{name}"))], &values("Kyjovice č. 2")).map(|p| targets(&p).remove(0))
}

// --- Parsing ---

#[test]
fn the_examples_parse() {
	for json in
		[include_str!("../examples/receipts-to-accounting.json"), include_str!("../examples/photos-by-date.json")]
	{
		parse(json).unwrap_or_else(|e| panic!("{e:?}"));
	}
	assert!(Recipe::schema().is_object());
}

#[test]
fn an_unknown_key_is_refused_with_its_path() {
	let mut v = base("{item.name}");
	v["action"]["force"] = json!(true);
	let e = errors(v);
	assert_eq!(e[0].path, "action.force", "{e:?}");
	assert!(e[0].message.contains("force"), "{e:?}");
	let mut v = base("{item.name}");
	v["extra"] = json!(1);
	assert!(errors(v)[0].message.contains("extra"));
}

#[test]
fn an_unknown_format_is_refused() {
	let mut v = base("{item.name}");
	v["format"] = json!("tpfs-recipe/2");
	assert_eq!(errors(v)[0].path, "format");
}

#[test]
fn form_keywords_outside_the_subset_are_refused() {
	for field in [
		json!({ "type": "object", "properties": {} }),
		json!({ "type": "array", "items": { "type": "string" } }),
		json!({ "type": "string", "if": {} }),
		json!({ "type": "string", "oneOf": [] }),
		json!({ "type": "integer", "multipleOf": 2 }),
		json!({ "type": "boolean", "enum": [true] }),
		json!({ "type": "string", "x-tpfs": { "run": true } }),
	] {
		let mut v = base("{item.name}");
		v["form"]["properties"]["bad"] = field.clone();
		let e = errors(v);
		assert!(e[0].path.starts_with("form.properties.bad"), "{field}: {e:?}");
	}
}

#[test]
fn every_problem_is_reported_with_its_key() {
	let mut v = base("{form.nope}");
	v["id"] = json!("X");
	v["version"] = json!(0);
	v["title"] = json!("a\u{202e}b");
	v["scope"]["roots"] = json!(["relative"]);
	v["input"]["max"] = json!(9999);
	v["form"]["properties"]["label"]["pattern"] = json!("(");
	let paths: Vec<String> = errors(v).into_iter().map(|e| e.path).collect();
	for want in ["id", "version", "title", "scope.roots[0]", "input", "form.properties.label.pattern", "action.to"] {
		assert!(paths.iter().any(|p| p == want), "{want} missing from {paths:?}");
	}
}

#[test]
fn templates_are_checked_when_parsed() {
	for (to, why) in [
		("", "a template"),
		("a/../b", "'..'"),
		("/abs/{item.name}", "empty path component"),
		("x\\{item.name}", "'\\'"),
		("{item.nope}", "unknown source"),
		("{item.name|upper}", "unknown filter"),
		("{item.name:%Y}", "takes no format"),
		("{today:%H}", "%H and %M"),
		("{n:3}", ":0N"),
		("{item.mtime:%Q}", "date format"),
		("x/{item.dir}/y", "{item.dir} can only start"),
		("x/{form.target}/y", "can only start"),
		("{form.target}", "needs a name"),
		("{item.name", "without '}'"),
	] {
		let e = errors(base(to));
		assert!(e.iter().any(|e| e.path == "action.to" && e.message.contains(why)), "{to}: {e:?}");
	}
	let mut v = base("{form.label}");
	v["form"]["required"] = json!([]);
	assert!(errors(v)[0].message.contains("form.required"));
	let mut v = base("a/{item.name}");
	v["action"]["op"] = json!("rename");
	assert!(errors(v)[0].message.contains("name only"));
}

// --- Templates ---

#[test]
fn every_source_format_and_filter() {
	assert_eq!(one("{item.dir}/x_{item.name}", "a.TXT").unwrap(), "/data/in/x_a.TXT");
	assert_eq!(one("{item.stem}-{item.ext|lower}.x", "Report.Final.PDF").unwrap(), "/data/in/Report.Final-pdf.x");
	assert_eq!(one("{item.mtime}_{item.mtime:%Y%m%d-%H%M}", ".bashrc").unwrap(), "/data/in/2025-12-31_20251231-2305");
	assert_eq!(one("{today}_{today:%d.%m.%Y}", "a").unwrap(), "/data/in/2026-11-20_20.11.2026");
	assert_eq!(one("{n}-{n:03}", "a").unwrap(), "/data/in/1-001");
	assert_eq!(one("{form.count:04}_{form.count}_{form.when:%Y}", "a").unwrap(), "/data/in/0007_7_2026");
	assert_eq!(one("{form.target}/{form.label|trim}", "a").unwrap(), "/data/out/Kyjovice č. 2");
	assert_eq!(one("{form.label|lower}", "a").unwrap(), "/data/in/kyjovice č. 2");
	assert_eq!(one("x{item.ext}y", "noext").unwrap(), "/data/in/xy");
}

#[test]
fn ascii_and_slug_on_czech_diacritics() {
	assert_eq!(one("{form.label|ascii}", "a").unwrap(), "/data/in/Kyjovice c. 2");
	assert_eq!(one("{form.label|ascii|slug}", "a").unwrap(), "/data/in/Kyjovice_c._2");
	assert_eq!(text::ascii("Příliš žluťoučký kůň úpěl ďábelské ódy"), "Prilis zlutoucky kun upel dabelske ody");
	assert_eq!(text::ascii("ŘÍŠE Ůdolí Straße Łódź"), "RISE Udoli Strasse Lodz");
	assert_eq!(text::slug("a  b//c::d\t?e"), "a_b_c_d_e");
	assert_eq!(text::slug("__x__"), "_x_");
}

#[test]
fn n_counts_in_selection_order() {
	let r = recipe(base("{n:02}_{item.name}"));
	let plan = run(&r, &[item("/data/b"), item("/data/a"), item("/data/c")], &values("x")).unwrap();
	assert_eq!(targets(&plan), ["/data/01_b", "/data/02_a", "/data/03_c"]);
}

#[test]
fn a_dir_field_starts_the_target_and_the_scope_root_may_be_it() {
	let r = recipe(base("{form.target}/{item.name}"));
	let mut v = values("x");
	v.0.insert("target".into(), json!("/data"));
	assert_eq!(targets(&run(&r, &[item("/data/in/a")], &v).unwrap()), ["/data/a"]);
}

// --- Cleaning ---

#[test]
fn a_value_never_makes_a_folder() {
	assert_eq!(one("{form.label}", "a").unwrap(), "/data/in/Kyjovice č. 2");
	let r = recipe(base("{form.label}.txt"));
	for (label, want) in [("../x", "/data/in/.._x.txt"), ("a/b", "/data/in/a_b.txt"), ("a\\b", "/data/in/a_b.txt")] {
		let plan = run(&r, &[item("/data/in/f")], &values(label)).unwrap();
		assert_eq!(targets(&plan), [want], "{label}");
		assert!(mkdirs(&plan).is_empty());
	}
}

#[test]
fn bad_names_are_refused() {
	let r = recipe(base("{form.label}"));
	for label in
		["..", ".", "CON", "nul.txt", "com1", "trailing.", "trailing ", "", "a\u{202e}b", "a\nb", &"x".repeat(300)]
	{
		let e = run(&r, &[item("/data/in/f")], &values(label)).expect_err(label);
		assert!(e[0].path == "items[0]" || e[0].path == "values.label", "{label:?}: {e:?}");
	}
	// The source's own name is checked too.
	let r = recipe(base("x_{item.name}"));
	assert!(run(&r, &[item("/data/in/a.")], &values("x")).is_err());
}

// --- Scope ---

#[test]
fn a_selection_outside_the_scope_is_refused_as_a_whole() {
	let r = recipe(base("{item.name}.bak"));
	let e = run(&r, &[item("/data/a"), item("/etc/passwd"), item("/data/../etc/x")], &values("x")).unwrap_err();
	let paths: Vec<_> = e.iter().map(|e| e.path.as_str()).collect();
	assert_eq!(paths, ["items[1]", "items[2]"]);
	// The root itself is not a selectable entry.
	assert!(run(&r, &[item("/data")], &values("x")).is_err());
	// Relative paths are refused.
	assert!(run(&r, &[item("data/a")], &values("x")).is_err());
}

#[test]
fn a_target_outside_the_scope_is_refused() {
	let r = recipe(base("{form.target}/{item.name}"));
	let mut v = values("x");
	v.0.insert("target".into(), json!("/elsewhere"));
	assert_eq!(run(&r, &[item("/data/a")], &v).unwrap_err()[0].path, "values.target");
	v.0.insert("target".into(), json!("/data/../elsewhere"));
	assert_eq!(run(&r, &[item("/data/a")], &v).unwrap_err()[0].path, "values.target");
	// A narrower confirmed scope wins over the recipe's own.
	let v = values("x");
	let e = expand(&r, &[PathBuf::from("/data/in")], &[item("/data/in/a")], &v, today(), &|_| false).unwrap_err();
	assert_eq!(e[0].path, "values.target");
}

#[test]
fn values_are_checked_against_the_form() {
	let mut v = base("{form.label}");
	v["form"]["properties"]["label"] =
		json!({ "type": "string", "minLength": 2, "maxLength": 5, "pattern": "^[a-z]+$" });
	let r = recipe(v);
	for (label, ok) in [("ab", true), ("a", false), ("abcdef", false), ("AB", false)] {
		assert_eq!(run(&r, &[item("/data/f")], &values(label)).is_ok(), ok, "{label}");
	}
	let mut v = values("ab");
	v.0.insert("count".into(), json!(1000));
	v.0.insert("when".into(), json!("2026-02-30"));
	v.0.insert("stray".into(), json!(1));
	v.0.remove("target");
	let paths: Vec<_> = run(&r, &[item("/data/f")], &v).unwrap_err().into_iter().map(|e| e.path).collect();
	for want in ["values.count", "values.when", "values.stray", "values.target"] {
		assert!(paths.iter().any(|p| p == want), "{want} missing from {paths:?}");
	}
}

#[test]
fn input_kind_glob_and_counts_are_enforced() {
	let mut v = base("{item.name}.x");
	v["input"] = json!({ "kind": "file", "glob": ["*.PDF", "scan-??.jpg"], "min": 2, "max": 3 });
	let r = recipe(v);
	let ok = [item("/data/a.pdf"), item("/data/scan-01.JPG")];
	assert!(run(&r, &ok, &values("x")).is_ok());
	assert!(run(&r, &ok[..1], &values("x")).is_err(), "below min");
	let four = [item("/data/a.pdf"), item("/data/b.pdf"), item("/data/c.pdf"), item("/data/d.pdf")];
	assert!(run(&r, &four, &values("x")).is_err(), "above max");
	let e = run(&r, &[item("/data/a.pdf"), item("/data/scan-1.jpg")], &values("x")).unwrap_err();
	assert_eq!(e[0].path, "items[1]");
	let folder = Item { dir: true, ..item("/data/b.pdf") };
	assert!(run(&r, &[item("/data/a.pdf"), folder], &values("x")).is_err(), "a folder for a file recipe");
	assert!(text::glob("*", "") && text::glob("a*b*c", "aXbYc") && !text::glob("a*b", "ab_"));
}

// --- Operations ---

#[test]
fn missing_folders_become_mkdirs_first_each_once() {
	let r = recipe(base("{form.target}/{item.mtime:%Y}/{item.mtime:%m}/{item.name}"));
	let items = [
		item("/data/in/a"),
		item("/data/in/b"),
		Item { mtime: Stamp::date(2024, 1, 2).unwrap(), ..item("/data/in/c") },
	];
	let plan = run(&r, &items, &values("x")).unwrap();
	assert_eq!(mkdirs(&plan), ["/data/out/2025", "/data/out/2025/12", "/data/out/2024", "/data/out/2024/01"]);
	assert!(matches!(plan.ops[..4], [Op::Mkdir { .. }, Op::Mkdir { .. }, Op::Mkdir { .. }, Op::Mkdir { .. }]));
	assert_eq!(targets(&plan), ["/data/out/2025/12/a", "/data/out/2025/12/b", "/data/out/2024/01/c"]);
	assert!(!plan.ops.iter().any(|o| matches!(o, Op::Trash { .. } | Op::Rmdir { .. })));
	// Existing folders are not created again; the base folder never is.
	let exists = |p: &Path| p == Path::new("/data/out/2025");
	let plan = expand(&r, &[PathBuf::from("/data")], &items[..1], &values("x"), today(), &exists).unwrap();
	assert_eq!(mkdirs(&plan), ["/data/out/2025/12"]);
}

#[test]
fn copy_and_rename_produce_their_ops() {
	let mut v = base("{form.target}/{item.name}");
	v["action"]["op"] = json!("copy");
	let plan = run(&recipe(v), &[item("/data/in/a")], &values("x")).unwrap();
	assert_eq!(plan.ops, [Op::Copy { from: "/data/in/a".into(), to: "/data/out/a".into() }]);
	let mut v = base("{n:02}_{item.name}");
	v["action"]["op"] = json!("rename");
	let plan = run(&recipe(v), &[item("/data/in/a")], &values("x")).unwrap();
	assert_eq!(plan.ops, [Op::Rename { path: "/data/in/a".into(), new_name: "01_a".into() }]);
	assert_eq!(plan.reason, "Recipe \"Test\" v2: rename 1 file");
}

#[test]
fn an_entry_already_at_its_target_is_skipped() {
	let mut v = base("{item.name}");
	v["action"]["op"] = json!("rename");
	let r = recipe(v);
	assert!(run(&r, &[item("/data/a")], &values("x")).is_err(), "nothing to do");
	let mut v = base("{item.name|lower}");
	v["action"]["op"] = json!("rename");
	let plan = run(&recipe(v), &[item("/data/A"), item("/data/b")], &values("x")).unwrap();
	assert_eq!(targets(&plan), ["a"], "a case change is a rename; b is skipped");
}

// --- Conflicts ---

#[test]
fn number_resolves_collisions_within_the_plan_and_on_disk() {
	let mut v = base("{form.target}/{form.label}.{item.ext}");
	v["action"]["on_conflict"] = json!("number");
	let r = recipe(v);
	let items = [item("/data/in/1.pdf"), item("/data/in/2.pdf"), item("/data/in/3.PDF"), item("/data/in/4.txt")];
	let exists = |p: &Path| p == Path::new("/data/out/doc.pdf");
	let plan = expand(&r, &[PathBuf::from("/data")], &items, &values("doc"), today(), &exists).unwrap();
	assert_eq!(
		targets(&plan),
		["/data/out/doc_2.pdf", "/data/out/doc_3.pdf", "/data/out/doc.PDF", "/data/out/doc.txt"]
	);
	let mut v = base("{form.label}");
	v["action"]["on_conflict"] = json!("number");
	let plan = run(&recipe(v), &[item("/data/x/1"), item("/data/x/2")], &values("same")).unwrap();
	assert_eq!(targets(&plan), ["/data/x/same", "/data/x/same_2"]);
}

#[test]
fn stop_refuses_and_lists_every_collision() {
	let r = recipe(base("{form.target}/{form.label}"));
	let items = [item("/data/in/1"), item("/data/in/2"), item("/data/in/3")];
	let e = run(&r, &items, &values("same")).unwrap_err();
	assert_eq!(e.len(), 2, "{e:?}");
	assert!(e.iter().all(|e| e.path == "action.on_conflict"));
	let exists = |p: &Path| p == Path::new("/data/out/one");
	let e = expand(&r, &[PathBuf::from("/data")], &items[..1], &values("one"), today(), &exists).unwrap_err();
	assert_eq!(e.len(), 1);
}

#[test]
fn another_selected_entry_counts_as_taken() {
	let mut v = base("{form.label}");
	v["action"]["op"] = json!("rename");
	let r = recipe(v);
	let e = run(&r, &[item("/data/a"), item("/data/b")], &values("b")).unwrap_err();
	assert_eq!(e.len(), 1, "a → b collides with the selected b, b → b is skipped: {e:?}");
}

// --- Determinism ---

#[test]
fn the_same_input_gives_the_same_plan() {
	let mut v = base("{form.target}/{item.mtime:%Y}/{form.label|ascii|slug}_{n:03}.{item.ext}");
	v["action"]["on_conflict"] = json!("number");
	let r = recipe(v);
	let items: Vec<Item> = (0..50).map(|i| item(&format!("/data/in/f{}.pdf", i % 7))).collect();
	let a = run(&r, &items, &values("Kyjovice č. 2"));
	let b = run(&r, &items, &values("Kyjovice č. 2"));
	assert_eq!(a.as_ref().map(tpfs_core::Plan::digest), b.as_ref().map(tpfs_core::Plan::digest));
	let r2 = parse(&serde_json::to_string(&r).unwrap()).unwrap();
	assert_eq!(r.hash(), r2.hash());
	let c = run(&r2, &items, &values("Kyjovice č. 2"));
	assert_eq!(a.map(|p| p.digest()), c.map(|p| p.digest()));
}

#[test]
fn stamps_are_validated() {
	assert_eq!(Stamp::parse("2024-02-29T13:45:10"), Stamp::new(2024, 2, 29, 13, 45));
	assert_eq!(Stamp::parse("2026-11-20 08:01"), Stamp::new(2026, 11, 20, 8, 1));
	for bad in ["2023-02-29", "2026-13-01", "2026-1-01", "2026-01-01T24:00", "+026-01-01", "2026-01-01T1:00"] {
		assert_eq!(Stamp::parse(bad), None, "{bad}");
	}
}

#[test]
fn unix_times_become_utc_stamps() {
	assert_eq!(Stamp::from_unix(0), Stamp::new(1970, 1, 1, 0, 0));
	assert_eq!(Stamp::from_unix(951_782_400 + 3_661), Stamp::new(2000, 2, 29, 1, 1));
	assert_eq!(Stamp::from_unix(1_795_132_800), Stamp::date(2026, 11, 20));
	assert_eq!(Stamp::from_unix(-86_400), Stamp::date(1969, 12, 31));
}

#[test]
fn scope_roots_resolve_home() {
	let mut v = base("{item.name}.x");
	v["scope"]["roots"] = json!(["~", "~/Docs", "/srv"]);
	let r = recipe(v);
	let roots = r.resolve_roots(Path::new("/home/u"));
	assert_eq!(roots, [PathBuf::from("/home/u"), PathBuf::from("/home/u/Docs"), PathBuf::from("/srv")]);
	for bad in ["~user", "docs", "/a/../b"] {
		let mut v = base("{item.name}.x");
		v["scope"]["roots"] = json!([bad]);
		assert_eq!(errors(v)[0].path, "scope.roots[0]", "{bad}");
	}
}
