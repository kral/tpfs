//! Exit codes and messages of the CLI, as scripts rely on them.

use std::{
	fs,
	path::Path,
	process::{Command, Output, Stdio},
};

fn tpfs(home: &Path, args: &[&str]) -> Output {
	Command::new(env!("CARGO_BIN_EXE_tpfs")).args(args).env("TPFS_HOME", home).stdin(Stdio::null()).output().unwrap()
}

fn code(o: &Output) -> i32 {
	o.status.code().unwrap()
}

#[test]
fn exit_codes_tell_what_happened() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, home) = (base.join("root"), base.join("home"));
	fs::create_dir(&root).unwrap();
	let r = root.to_str().unwrap();

	let good = base.join("good.json");
	fs::write(&good, format!(r#"{{"ops":[{{"op":"mkdir","path":{:?}}}]}}"#, root.join("new"))).unwrap();
	let bad = base.join("bad.json");
	fs::write(&bad, format!(r#"{{"ops":[{{"op":"trash","path":{:?}}}]}}"#, root.join("missing"))).unwrap();
	let (g, b) = (good.to_str().unwrap(), bad.to_str().unwrap());

	assert_eq!(code(&tpfs(&home, &["check", g, "--root", r])), 0);
	assert_eq!(code(&tpfs(&home, &["check", b, "--root", r])), 3);
	assert_eq!(code(&tpfs(&home, &["check", g])), 2, "missing --root is a usage error");
	assert_eq!(code(&tpfs(&home, &["log", "extra"])), 2);
	assert_eq!(code(&tpfs(&home, &["undo", "--json"])), 2);

	// Not interactive and no digest: cancelled, nothing changed.
	let o = tpfs(&home, &["apply", g, "--root", r]);
	assert_eq!(code(&o), 4, "{}", String::from_utf8_lossy(&o.stderr));
	let o = tpfs(&home, &["apply", g, "--root", r, "--approve", "0000"]);
	assert_eq!(code(&o), 4);
	assert!(!root.join("new").exists());

	// The digest from `check` applies exactly that plan.
	let out = String::from_utf8(tpfs(&home, &["check", g, "--root", r]).stdout).unwrap();
	let digest = out.lines().find_map(|l| l.strip_prefix("Digest: ")).unwrap();
	assert_eq!(code(&tpfs(&home, &["apply", g, "--root", r, "--approve", digest])), 0);
	assert!(root.join("new").is_dir());
	// Applying it again is rejected: the folder exists now.
	assert_eq!(code(&tpfs(&home, &["apply", g, "--root", r, "--approve", digest])), 3);
}

#[test]
fn ui_without_a_terminal_fails_cleanly() {
	let tmp = tempfile::TempDir::new().unwrap();
	let o = tpfs(tmp.path(), &["ui", tmp.path().to_str().unwrap()]);
	assert_eq!(code(&o), 1);
	let err = String::from_utf8_lossy(&o.stderr);
	assert!(err.contains("interactive terminal") && !err.contains("panicked"), "{err}");
}

#[test]
fn log_does_not_pass_escape_sequences_to_the_terminal() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, home) = (base.join("root"), base.join("home"));
	fs::create_dir(&root).unwrap();
	let plan = base.join("p.json");
	let reason = "\u{1b}]52;c;aGk=\u{7}\u{1b}[2J";
	fs::write(
		&plan,
		serde_json::json!({ "reason": reason, "ops": [{ "op": "mkdir", "path": root.join("d") }] }).to_string(),
	)
	.unwrap();
	let (p, r) = (plan.to_str().unwrap(), root.to_str().unwrap());
	let out = String::from_utf8(tpfs(&home, &["check", p, "--root", r]).stdout).unwrap();
	let digest = out.lines().find_map(|l| l.strip_prefix("Digest: ")).unwrap().to_owned();
	assert!(!out.contains('\u{1b}'));
	assert_eq!(code(&tpfs(&home, &["apply", p, "--root", r, "--approve", &digest])), 0);
	let log = tpfs(&home, &["log"]).stdout;
	assert!(!log.contains(&0x1b) && !log.contains(&0x07), "{}", String::from_utf8_lossy(&log));
}
