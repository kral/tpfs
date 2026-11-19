//! `tpfs sync` as a script would use it: dry run, approve the printed digest, verify, undo.

use std::{
	fs,
	path::Path,
	process::{Command, Output, Stdio},
};

fn tpfs(home: &Path, args: &[&str]) -> Output {
	Command::new(env!("CARGO_BIN_EXE_tpfs")).args(args).env("TPFS_HOME", home).stdin(Stdio::null()).output().unwrap()
}

fn stdout(o: &Output) -> String {
	String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn dry_run_approve_digest_and_undo() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (src, dst, home) = (base.join("src"), base.join("dst"), base.join("state"));
	fs::create_dir_all(src.join("album")).unwrap();
	fs::create_dir_all(&dst).unwrap();
	fs::write(src.join("album/a.jpg"), "a").unwrap();
	fs::write(dst.join("extra.txt"), "x").unwrap();
	let (s, d) = (src.to_str().unwrap(), dst.to_str().unwrap());

	// Dry run changes nothing and prints a digest.
	let out = tpfs(&home, &["sync", s, d]);
	assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
	let text = stdout(&out);
	assert!(text.contains("Dry run"), "{text}");
	assert!(!dst.join("album").exists());
	let digest = text.lines().find_map(|l| l.strip_prefix("Digest: ")).unwrap().to_owned();

	// Without a terminal, --apply alone is refused.
	let out = tpfs(&home, &["sync", s, d, "--apply"]);
	assert!(!out.status.success());
	assert!(!dst.join("album").exists());

	// A digest from a different plan is refused.
	let out = tpfs(&home, &["sync", s, d, "--mirror", "--approve", &digest]);
	assert!(!out.status.success());
	assert!(dst.join("extra.txt").exists());

	// The digest from the dry run applies exactly that plan.
	let out = tpfs(&home, &["sync", s, d, "--approve", &digest]);
	assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
	assert_eq!(fs::read_to_string(dst.join("album/a.jpg")).unwrap(), "a");
	assert!(dst.join("extra.txt").exists(), "no mirror: extras stay");
	assert!(stdout(&tpfs(&home, &["sync", s, d])).contains("Nothing to sync"));

	// Undo through the shared journal.
	let id = {
		let mut j = tpfs_core::Journal::open(home.join("journal.jsonl")).unwrap();
		j.batches().unwrap().pop().unwrap().id
	};
	let out = tpfs(&home, &["undo", "--approve", &id]);
	assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
	assert!(!dst.join("album").exists());
}

#[test]
fn json_output_is_a_plan_for_apply() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (src, dst) = (base.join("src"), base.join("dst"));
	fs::create_dir_all(&src).unwrap();
	fs::create_dir_all(&dst).unwrap();
	fs::write(src.join("a.txt"), "a").unwrap();
	let out = tpfs(&base.join("state"), &["sync", src.to_str().unwrap(), dst.to_str().unwrap(), "--json"]);
	let plan: tpfs_core::Plan = serde_json::from_slice(&out.stdout).unwrap();
	assert_eq!(plan.ops.len(), 1);

	let nested = tpfs(&base.join("state"), &["sync", base.to_str().unwrap(), src.to_str().unwrap()]);
	assert!(!nested.status.success(), "nested folders are refused");
}
