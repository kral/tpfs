//! Windows paths (the other tests use Unix paths, which are not absolute on Windows).

use std::path::PathBuf;

use serde_json::json;
use tpfs_core::Op;

use super::*;

fn recipe() -> Recipe {
	from_value(json!({
		"format": FORMAT, "id": "win-test", "version": 1, "title": "Test",
		"scope": { "roots": [r"Y:\Doklady"] },
		"input": { "kind": "file" },
		"form": { "type": "object", "properties": { "target": { "type": "string", "format": "x-tpfs-dir" } }, "required": ["target"] },
		"action": { "op": "move", "to": "{form.target}/{item.mtime:%Y}/{item.name}" }
	}))
	.unwrap()
}

fn values(target: String) -> Values {
	Values([("target".to_owned(), json!(target))].into())
}

fn item(path: String) -> Item {
	Item { path: path.into(), dir: false, mtime: Stamp::new(2025, 12, 31, 23, 5).unwrap() }
}

fn today() -> Stamp {
	Stamp::date(2026, 11, 20).unwrap()
}

fn split(plan: &tpfs_core::Plan) -> (Vec<String>, Vec<String>) {
	let (mut dirs, mut targets) = (Vec::new(), Vec::new());
	for op in &plan.ops {
		match op {
			Op::Mkdir { path } => dirs.push(path.to_string_lossy().into_owned()),
			Op::Move { to, .. } => targets.push(to.to_string_lossy().into_owned()),
			_ => {}
		}
	}
	(dirs, targets)
}

#[test]
fn windows_paths() {
	let r = recipe();
	for root in [r"Y:\Doklady", r"\\NAS326\photo"] {
		let v = values(format!(r"{root}\out"));
		let items = [item(format!(r"{root}\in\a.pdf"))];
		let plan = expand(&r, &[PathBuf::from(root)], &items, &v, today(), &|_| false).unwrap();
		assert_eq!(split(&plan), (vec![format!(r"{root}\out\2025")], vec![format!(r"{root}\out\2025\a.pdf")]));
		// Another drive or share is outside.
		let outside = [item(r"Z:\in\a.pdf".to_owned())];
		assert!(expand(&r, &[PathBuf::from(root)], &outside, &v, today(), &|_| false).is_err());
	}
	// Letter case of the root does not matter on Windows.
	let items = [item(r"y:\doklady\in\a.pdf".to_owned())];
	let v = values(r"Y:\Doklady\out".to_owned());
	assert!(expand(&r, &[PathBuf::from(r"Y:\Doklady")], &items, &v, today(), &|_| false).is_ok());
}
