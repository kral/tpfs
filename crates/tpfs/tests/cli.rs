//! Exit codes and messages of the CLI, as scripts rely on them.

use std::{
	fs,
	path::Path,
	process::{Command, Output, Stdio},
};

fn tpfs(home: &Path, args: &[&str]) -> Output {
	// Spawned while no journal is open in this process: a child shares the locked file
	// description until it execs, which would make its own journal open fail.
	let child = {
		let _quiet = journal_lock();
		Command::new(env!("CARGO_BIN_EXE_tpfs"))
			.args(args)
			.env("TPFS_HOME", home)
			.stdin(Stdio::null())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap()
	};
	child.wait_with_output().unwrap()
}

/// The newest committed batch (the journal is opened only while no child runs).
fn newest_batch(home: &Path) -> Option<String> {
	let _quiet = journal_lock();
	let mut j = tpfs_core::Journal::open(home.join("journal.jsonl")).unwrap();
	let b = j.batches().unwrap().into_iter().rev().find(|b| b.status == tpfs_core::BatchStatus::Committed);
	b.map(|b| b.id)
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
	assert_eq!(code(&tpfs(&home, &["log", "extra"])), 2, "an unknown batch is a usage error");
	assert_eq!(code(&tpfs(&home, &["log", "a", "b"])), 2);
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

#[test]
fn agent_plans_are_approved_only_in_a_terminal_and_can_be_rejected() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, home) = (base.join("root"), base.join("home"));
	fs::create_dir(&root).unwrap();
	let mut server = tpfs_mcp::Server::new(tpfs_mcp::Config {
		roots: vec![root.clone()],
		state: home.clone(),
		approval: tpfs_mcp::Approval::Terminal,
	})
	.unwrap();
	let msg = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "propose_plan",
		"arguments": { "plan": { "ops": [{ "op": "mkdir", "path": root.join("d") }] } } } });
	let r = server.handle(&msg.to_string()).unwrap();
	let v: serde_json::Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
	let id = v["planId"].as_str().unwrap().to_owned();

	assert!(String::from_utf8(tpfs(&home, &["pending"]).stdout).unwrap().contains(&id));
	assert_eq!(code(&tpfs(&home, &["approve", &id])), 4, "no terminal, no approval");
	assert!(!server.store().is_approved(&server.store().get(&id).unwrap()));
	assert_eq!(code(&tpfs(&home, &["reject", &id])), 0);
	assert!(server.store().get(&id).is_err());
	assert_eq!(code(&tpfs(&home, &["reject", "deadbeef"])), 2, "an unknown id is a usage error");
}

#[test]
fn recover_reports_success_and_refusal_by_exit_code() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, home) = (base.join("root"), base.join("home"));
	fs::create_dir(&root).unwrap();
	assert_eq!(code(&tpfs(&home, &["recover"])), 0, "nothing to recover");

	// An interrupted batch whose rollback is blocked by a file in the way.
	let a = root.join("a.txt");
	let b = root.join("b.txt");
	fs::write(&b, "moved").unwrap();
	fs::write(&a, "in the way").unwrap();
	{
		use tpfs_core::{Entry, Journal, Op};
		let _quiet = journal_lock();
		let mut j = Journal::open(home.join("journal.jsonl")).unwrap();
		j.append(&Entry::Begin {
			batch: "x".into(),
			at: 0,
			source: "human".into(),
			reason: String::new(),
			digest: String::new(),
			v: 1,
		})
		.unwrap();
		j.append(&Entry::Intent {
			batch: "x".into(),
			index: 0,
			forward: Op::Move { from: a.clone(), to: b.clone() },
			inverse: Op::Move { from: b.clone(), to: a.clone() },
		})
		.unwrap();
		j.append(&Entry::Done { batch: "x".into(), index: 0, check: None, mode: None }).unwrap();
	}
	assert_eq!(code(&tpfs(&home, &["recover"])), 1);
	fs::remove_file(&a).unwrap();
	assert_eq!(code(&tpfs(&home, &["recover"])), 0);
	assert_eq!(fs::read_to_string(&a).unwrap(), "moved");
}

#[test]
fn two_way_sync_by_digest_is_idempotent_and_undoable() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (l, r, home) = (base.join("l"), base.join("r"), base.join("home"));
	fs::create_dir_all(&l).unwrap();
	fs::create_dir_all(&r).unwrap();
	fs::write(l.join("only-l.txt"), "L").unwrap();
	fs::write(r.join("only-r.txt"), "R").unwrap();
	let (ls, rs) = (l.to_str().unwrap(), r.to_str().unwrap());
	for mode in [&["--both"][..], &["--mirror"][..]] {
		let mut args = vec!["sync", ls, rs];
		args.extend_from_slice(mode);
		let out = String::from_utf8(tpfs(&home, &args).stdout).unwrap();
		if let Some(digest) = out.lines().find_map(|l| l.strip_prefix("Digest: ")) {
			let mut apply = args.clone();
			apply.extend_from_slice(&["--approve", digest]);
			assert_eq!(code(&tpfs(&home, &apply)), 0, "{out}");
		}
		let again = String::from_utf8(tpfs(&home, &args).stdout).unwrap();
		assert!(again.contains("Nothing to sync"), "{mode:?} is not idempotent:\n{again}");
	}
	assert_eq!(code(&tpfs(&home, &["undo"])), 4, "not interactive and no batch named: nothing undone");
	assert_eq!(code(&tpfs(&home, &["undo", "--approve", "nope"])), 4);
	while let Some(id) = newest_batch(&home) {
		assert_eq!(code(&tpfs(&home, &["undo", "--approve", &id])), 0);
	}
	assert!(l.join("only-l.txt").exists() && !l.join("only-r.txt").exists());
	assert!(r.join("only-r.txt").exists() && !r.join("only-l.txt").exists());
}

/// `tpfs mcp` over stdio: only protocol on stdout, a clean exit at the end of input.
#[test]
fn mcp_speaks_only_json_rpc_on_stdout() {
	use std::io::Write;
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, home) = (base.join("root"), base.join("home"));
	fs::create_dir(&root).unwrap();
	let quiet = journal_lock();
	let mut child = Command::new(env!("CARGO_BIN_EXE_tpfs"))
		.args(["mcp", "--root", root.to_str().unwrap()])
		.env("TPFS_HOME", &home)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()
		.unwrap();
	drop(quiet);
	let mut stdin = child.stdin.take().unwrap();
	stdin
		.write_all(
			b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}",
		)
		.unwrap();
	drop(stdin);
	let out = child.wait_with_output().unwrap();
	assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
	let stdout = String::from_utf8(out.stdout).unwrap();
	let lines: Vec<&str> = stdout.lines().collect();
	assert_eq!(lines.len(), 2, "the last line without a newline is answered too: {stdout}");
	for (i, l) in lines.iter().enumerate() {
		assert!(
			l.starts_with('{') && l.contains("\"jsonrpc\":\"2.0\"") && l.contains(&format!("\"id\":{}", i + 1)),
			"{l}"
		);
	}
	assert!(String::from_utf8_lossy(&out.stderr).contains("serving on stdio"));
}

#[test]
fn hostile_plan_files_print_no_terminal_controls() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let plan = base.join("evil.json");
	fs::write(&plan, "{\"ops\":[{\"op\":\"\\u001b]52;c;eA==\\u0007\\u202eOK\"}]}").unwrap();
	let o = tpfs(&base.join("home"), &["check", plan.to_str().unwrap(), "--root", base.to_str().unwrap()]);
	assert_eq!(code(&o), 3, "{}", String::from_utf8_lossy(&o.stderr));
	let err = String::from_utf8(o.stderr).unwrap();
	assert!(!err.trim_end().chars().any(|c| c.is_control() || c == '\u{202e}'), "{err:?}");
}

#[test]
fn a_missing_option_value_is_a_usage_error() {
	let tmp = tempfile::TempDir::new().unwrap();
	let home = tmp.path().join("home");
	for flag in ["--root", "--approve", "--home", "--source", "--approval", "--max-ops"] {
		assert_eq!(code(&tpfs(&home, &["apply", "p.json", flag])), 2, "{flag}");
	}
}

#[test]
fn an_empty_plan_is_nothing_to_do_and_undo_points_to_recover_first() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, home) = (base.join("root"), base.join("home"));
	fs::create_dir(&root).unwrap();
	let empty = base.join("empty.json");
	fs::write(&empty, r#"{"ops":[]}"#).unwrap();
	let o = tpfs(&home, &["apply", empty.to_str().unwrap(), "--root", root.to_str().unwrap()]);
	assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
	assert!(String::from_utf8_lossy(&o.stdout).contains("Nothing to do"));

	// An interrupted batch on top: undo names it and points to recover, without asking.
	{
		use tpfs_core::{Entry, Journal, Op};
		tpfs_core::private_dir(&home).unwrap();
		let _quiet = journal_lock();
		let mut j = Journal::open(home.join("journal.jsonl")).unwrap();
		let dir = root.join("d");
		j.append(&Entry::Begin {
			batch: "1-cut".into(),
			at: 0,
			source: "human".into(),
			reason: String::new(),
			digest: String::new(),
			v: 1,
		})
		.unwrap();
		j.append(&Entry::Intent {
			batch: "1-cut".into(),
			index: 0,
			forward: Op::Mkdir { path: dir.clone() },
			inverse: Op::Rmdir { path: dir },
		})
		.unwrap();
	}
	let o = tpfs(&home, &["undo"]);
	assert_eq!(code(&o), 1);
	let err = String::from_utf8_lossy(&o.stderr);
	assert!(err.contains("1-cut") && err.contains("tpfs recover"), "{err}");
}

#[test]
fn log_lists_the_steps_of_one_batch() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, home) = (base.join("root"), base.join("home"));
	fs::create_dir(&root).unwrap();
	let plan = base.join("p.json");
	fs::write(
		&plan,
		format!(
			r#"{{"ops":[{{"op":"mkdir","path":{:?}}},{{"op":"mkdir","path":{:?}}}]}}"#,
			root.join("a"),
			root.join("b")
		),
	)
	.unwrap();
	let digest =
		String::from_utf8(tpfs(&home, &["check", plan.to_str().unwrap(), "--root", root.to_str().unwrap()]).stdout)
			.unwrap()
			.lines()
			.find_map(|l| l.strip_prefix("Digest: ").map(str::to_owned))
			.unwrap();
	assert_eq!(
		code(&tpfs(&home, &["apply", plan.to_str().unwrap(), "--root", root.to_str().unwrap(), "--approve", &digest])),
		0
	);
	let id = newest_batch(&home).unwrap();
	let o = tpfs(&home, &["log", &id]);
	assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
	let out = String::from_utf8(o.stdout).unwrap();
	assert!(out.contains("#1") && out.contains("#2") && out.matches("undo:").count() == 2, "{out}");
	assert_eq!(code(&tpfs(&home, &["log", "nope"])), 2);
}

/// Archived history: `tpfs log` says it is there, `--all` lists it, and one of its batches can
/// still be shown.
#[test]
fn log_shows_archived_history_on_request() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, home) = (base.join("root"), base.join("home"));
	fs::create_dir(&root).unwrap();
	let id = {
		let _quiet = journal_lock();
		let v = tpfs_core::Validator::new([&root]).unwrap();
		let mut j = tpfs_core::Journal::open(home.join("journal.jsonl")).unwrap();
		let plan = tpfs_core::Plan::new(vec![tpfs_core::Op::Mkdir { path: root.join("a") }], "made and undone");
		let id = tpfs_core::Executor::new(&v, &mut j, home.join("trash"))
			.execute(&plan, tpfs_core::Source::Human)
			.unwrap()
			.id;
		tpfs_core::Executor::new(&v, &mut j, home.join("trash")).undo_last().unwrap();
		assert_eq!(j.compact().unwrap(), 1);
		id
	};
	let o = tpfs(&home, &["log"]);
	assert!(String::from_utf8_lossy(&o.stdout).contains("No batches"));
	assert!(String::from_utf8_lossy(&o.stderr).contains("--all"), "{}", String::from_utf8_lossy(&o.stderr));
	let o = tpfs(&home, &["log", "--all"]);
	let out = String::from_utf8_lossy(&o.stdout);
	assert!(out.contains(&id) && out.contains("undone"), "{out}");
	let o = tpfs(&home, &["log", &id]);
	assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
}

/// Held while a journal is opened in this process or a child is spawned (see `tpfs`).
fn journal_lock() -> std::sync::MutexGuard<'static, ()> {
	static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
	LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn a_damaged_pending_plan_is_listed_and_can_be_rejected() {
	let tmp = tempfile::TempDir::new().unwrap();
	let home = tpfs_core::canonicalize(tmp.path()).unwrap().join("home");
	tpfs_core::private_dir(&home.join("pending")).unwrap();
	let file = home.join("pending").join("abcd1234.json");
	fs::write(&file, "garbage").unwrap();
	let out = String::from_utf8(tpfs(&home, &["pending"]).stdout).unwrap();
	assert!(out.contains("abcd1234") && out.contains("DAMAGED"), "{out}");
	assert_eq!(code(&tpfs(&home, &["reject", "abcd1234"])), 0);
	assert!(!file.exists());
}

/// `tpfs mcp` refuses a state folder others can write to before any plan is proposed, not
/// after a human approved one that could then not be applied.
#[cfg(unix)]
#[test]
fn mcp_refuses_a_shared_state_folder_at_start() {
	use std::os::unix::fs::PermissionsExt;
	let tmp = tempfile::TempDir::new().unwrap();
	let home = tmp.path().join("home");
	fs::create_dir(&home).unwrap();
	fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
	let o = tpfs(&home, &["mcp", "--root", tmp.path().to_str().unwrap()]);
	assert_ne!(code(&o), 0);
	let err = String::from_utf8_lossy(&o.stderr);
	assert!(err.contains("chmod 700") && !err.contains("serving"), "{err}");
	assert!(o.stdout.is_empty());
}

/// `tpfs guard` as Claude Code runs it: the tool call on stdin, a refusal on stdout for a shell
/// command that deletes files, nothing for anything else, exit 0 either way.
#[test]
fn guard_refuses_file_deleting_shell_commands() {
	use std::io::Write;
	let run = |input: &str| {
		let mut child = Command::new(env!("CARGO_BIN_EXE_tpfs"))
			.arg("guard")
			.stdin(Stdio::piped())
			.stdout(Stdio::piped())
			.stderr(Stdio::piped())
			.spawn()
			.unwrap();
		child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
		child.wait_with_output().unwrap()
	};
	let o = run(
		r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf tests/ patches/ plan/ ~/"}}"#,
	);
	assert_eq!(code(&o), 0);
	let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
	assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
	let o = run(r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"}}"#);
	assert_eq!(code(&o), 0);
	assert!(o.stdout.is_empty());
}

/// `tpfs watch` reviews plans in a terminal only: without one it refuses (exit 4) and approves
/// nothing.
#[test]
fn watch_needs_a_terminal() {
	let tmp = tempfile::TempDir::new().unwrap();
	let o = tpfs(&tmp.path().join("home"), &["watch"]);
	assert_eq!(code(&o), 4, "{}", String::from_utf8_lossy(&o.stderr));
	assert!(String::from_utf8_lossy(&o.stderr).contains("terminal"));
}

/// Recipes from the command line: list, reject, a refused non-interactive install, and runs on a
/// selection, approved by digest.
#[test]
fn recipes_are_listed_rejected_and_run() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, home) = (base.join("photos"), base.join("home"));
	fs::create_dir(&root).unwrap();
	fs::write(root.join("IMG_1.JPG"), "a").unwrap();
	fs::write(root.join("notes.txt"), "b").unwrap();
	let mut json: serde_json::Value =
		serde_json::from_str(include_str!("../../tpfs-recipe/examples/photos-by-date.json")).unwrap();
	json["scope"]["roots"] = serde_json::json!([root]);
	let recipe = tpfs_recipe::from_value(json).unwrap();

	let o = tpfs(&home, &["recipes"]);
	assert!(String::from_utf8_lossy(&o.stdout).contains("No recipes"));
	let inbox = tpfs_mcp::RecipeStore::new(&home);
	let a = inbox.put(&recipe, "test-agent", "for the holiday").unwrap();
	let mut newer = recipe.clone();
	newer.version += 1;
	let b = inbox.put(&newer, "test-agent", "").unwrap();
	let out = String::from_utf8_lossy(&tpfs(&home, &["recipes"]).stdout).into_owned();
	assert!(out.contains(&a.id) && out.contains("test-agent") && out.contains("for the holiday"), "{out}");

	// Installing is interactive only.
	let o = tpfs(&home, &["recipe", "install", &a.id]);
	assert_eq!(code(&o), 4, "{}", String::from_utf8_lossy(&o.stderr));
	assert!(String::from_utf8_lossy(&o.stdout).contains("Hash: "), "the recipe is shown first");
	assert_eq!(inbox.status(&a.id), tpfs_mcp::RecipeStatus::Pending);
	assert_eq!(code(&tpfs(&home, &["recipe", "reject", &b.id])), 0);
	assert_eq!(inbox.status(&b.id), tpfs_mcp::RecipeStatus::Rejected);
	assert_eq!(code(&tpfs(&home, &["recipe", "reject", &b.id])), 2, "already decided");
	assert_eq!(code(&tpfs(&home, &["recipe", "install", &a.id, "--set", "x=y"])), 2);
	assert_eq!(code(&tpfs(&home, &["recipe", "frobnicate", "x"])), 2);

	// Installed as the person would in a terminal (same calls as `tpfs recipe install`).
	tpfs_mcp::InstalledRecipes::new(&home).install(&recipe, &a.hash, std::slice::from_ref(&root)).unwrap();
	inbox.mark_installed(&a.id, &a.hash).unwrap();
	let img = root.join("IMG_1.JPG");
	let img = img.to_str().unwrap();
	let run = |extra: &[&str]| {
		let mut v = vec!["recipe", "run", "photos-by-date", img];
		v.extend_from_slice(extra);
		tpfs(&home, &v)
	};
	assert_eq!(code(&run(&[])), 2, "a required field is missing and nobody can be asked");
	let txt = root.join("notes.txt");
	let o = tpfs(&home, &["recipe", "run", "photos-by-date", txt.to_str().unwrap(), "--set", "place=Kyjovice"]);
	assert_eq!(code(&o), 3, "the glob refuses notes.txt: {}", String::from_utf8_lossy(&o.stderr));
	let o = run(&["--set", "place=Kyjovice č. 2"]);
	assert_eq!(code(&o), 4, "a new recipe asks; not interactive and no digest");
	let out = String::from_utf8_lossy(&o.stdout).into_owned();
	assert!(out.contains("kyjovice_c._2.jpg") && out.contains("new to you"), "{out}");
	let digest = out.lines().find_map(|l| l.strip_prefix("Digest: ")).unwrap().to_owned();
	// During its first runs a recipe needs a typed yes, even with the digest.
	let o = run(&["--set", "place=Kyjovice č. 2", "--approve", &digest]);
	assert_eq!(code(&o), 4, "{}", String::from_utf8_lossy(&o.stderr));
	let shelf = tpfs_mcp::InstalledRecipes::new(&home);
	for _ in 0..3 {
		shelf.record_run("photos-by-date").unwrap();
	}
	let o = run(&["--set", "place=Kyjovice č. 2", "--approve", &digest]);
	assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
	assert!(!root.join("IMG_1.JPG").exists());
	let renamed: Vec<_> = fs::read_dir(&root).unwrap().filter_map(|e| e.ok()?.file_name().into_string().ok()).collect();
	assert!(renamed.iter().any(|n| n.ends_with("_kyjovice_c._2.jpg")), "{renamed:?}");
	assert_eq!(shelf.get("photos-by-date").unwrap().runs, 4);
	// The run is an ordinary batch: undo puts the name back.
	let batch = newest_batch(&home).unwrap();
	assert_eq!(code(&tpfs(&home, &["undo", "--approve", &batch])), 0);
	assert!(root.join("IMG_1.JPG").exists());
	assert_eq!(code(&tpfs(&home, &["recipe", "remove", "photos-by-date"])), 0);
	assert_eq!(code(&tpfs(&home, &["recipe", "run", "photos-by-date", img])), 2);
}

/// A recipe for a whole drive is refused at install; `show` of an unknown id says so plainly.
#[test]
fn a_recipe_for_a_whole_drive_is_not_installed() {
	let tmp = tempfile::TempDir::new().unwrap();
	let home = tpfs_core::canonicalize(tmp.path()).unwrap().join("home");
	let mut json: serde_json::Value =
		serde_json::from_str(include_str!("../../tpfs-recipe/examples/photos-by-date.json")).unwrap();
	json["scope"]["roots"] = serde_json::json!(["/"]);
	let p = tpfs_mcp::RecipeStore::new(&home).put(&tpfs_recipe::from_value(json).unwrap(), "agent", "").unwrap();
	let o = tpfs(&home, &["recipe", "install", &p.id]);
	assert_eq!(code(&o), 1);
	assert!(String::from_utf8_lossy(&o.stderr).contains("whole drive"), "{}", String::from_utf8_lossy(&o.stderr));
	let o = tpfs(&home, &["recipe", "show", "nonexistent"]);
	assert!(
		String::from_utf8_lossy(&o.stderr).contains("no recipe or proposal"),
		"{}",
		String::from_utf8_lossy(&o.stderr)
	);
}

/// A run whose target folder is a link out of the recipe's folders is refused by validation, and
/// nothing appears outside.
#[cfg(unix)]
#[test]
fn a_recipe_run_through_a_link_out_of_its_folders_is_refused() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, outside, home) = (base.join("root"), base.join("outside"), base.join("home"));
	fs::create_dir_all(&root).unwrap();
	fs::create_dir_all(&outside).unwrap();
	std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
	fs::write(root.join("a.pdf"), "a").unwrap();
	let recipe = tpfs_recipe::from_value(serde_json::json!({
		"format": "tpfs-recipe/1", "id": "file-it", "version": 1, "title": "File it",
		"scope": { "roots": [root] },
		"input": { "kind": "file" },
		"form": { "type": "object", "properties": { "target": { "type": "string", "format": "x-tpfs-dir" } }, "required": ["target"] },
		"action": { "op": "move", "to": "{form.target}/{item.name}" }
	}))
	.unwrap();
	let shelf = tpfs_mcp::InstalledRecipes::new(&home);
	shelf.install(&recipe, &recipe.hash(), std::slice::from_ref(&root)).unwrap();
	for _ in 0..3 {
		shelf.record_run("file-it").unwrap();
	}
	let target = format!("target={}", root.join("out").display());
	let a = root.join("a.pdf");
	let o = tpfs(&home, &["recipe", "run", "file-it", a.to_str().unwrap(), "--set", &target, "--approve", "x"]);
	assert_eq!(code(&o), 3, "refused by validation, before any approval: {}", String::from_utf8_lossy(&o.stdout));
	assert!(a.exists());
	assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "something reached the folder outside");
}

/// `tpfs trash` lists; `tpfs trash empty` deletes only the selection whose token was shown, by
/// age (30 days unless told otherwise) or by batch; an emptied batch can no longer be undone.
#[test]
fn the_trash_is_emptied_only_by_its_token() {
	let tmp = tempfile::TempDir::new().unwrap();
	let base = tpfs_core::canonicalize(tmp.path()).unwrap();
	let (root, home) = (base.join("root"), base.join("home"));
	fs::create_dir(&root).unwrap();
	let r = root.to_str().unwrap();
	let mut ids = Vec::new();
	for name in ["a.txt", "b.txt"] {
		fs::write(root.join(name), name).unwrap();
		let plan = base.join("plan.json");
		fs::write(&plan, format!(r#"{{"ops":[{{"op":"trash","path":{:?}}}]}}"#, root.join(name))).unwrap();
		let p = plan.to_str().unwrap();
		let out = String::from_utf8(tpfs(&home, &["check", p, "--root", r]).stdout).unwrap();
		let digest = out.lines().find_map(|l| l.strip_prefix("Digest: ")).unwrap().to_owned();
		assert_eq!(code(&tpfs(&home, &["apply", p, "--root", r, "--approve", &digest])), 0);
		ids.push(newest_batch(&home).unwrap());
	}
	let stdout = |o: &Output| String::from_utf8_lossy(&o.stdout).into_owned();
	let token = |o: &Output| stdout(o).lines().find_map(|l| l.strip_prefix("Token: ")).unwrap().to_owned();

	let list = tpfs(&home, &["trash"]);
	assert_eq!(code(&list), 0);
	assert!(stdout(&list).contains(&ids[0]) && stdout(&list).contains("Total: 2 batch(es), 2 item(s), 10 B"));
	assert_eq!(code(&tpfs(&home, &["trash", "--batch", &ids[0]])), 2, "a filter needs `empty`");
	assert_eq!(code(&tpfs(&home, &["trash", "purge"])), 2);
	assert_eq!(code(&tpfs(&home, &["trash", "empty", "--batch", "nope"])), 2);
	assert_eq!(code(&tpfs(&home, &["trash", "empty", "--older-than", "soon"])), 2);

	// Nothing is 30 days old yet.
	let o = tpfs(&home, &["trash", "empty"]);
	assert!(code(&o) == 0 && stdout(&o).contains("Nothing to empty"), "{}", stdout(&o));

	// One batch: not interactive, so the token is needed, and only the right one works.
	let o = tpfs(&home, &["trash", "empty", "--batch", &ids[1]]);
	assert_eq!(code(&o), 4);
	assert!(stdout(&o).contains(root.join("b.txt").to_str().unwrap()), "shows where each item came from");
	let t = token(&o);
	assert_eq!(code(&tpfs(&home, &["trash", "empty", "--batch", &ids[1], "--approve", &ids[0]])), 4);
	assert!(home.join("trash").join(&ids[1]).exists());
	let o = tpfs(&home, &["trash", "empty", "--batch", &ids[1], "--approve", &t]);
	assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
	assert!(!home.join("trash").join(&ids[1]).exists() && home.join("trash").join(&ids[0]).exists());

	// Undo refuses the emptied batch and says why; the log says so too.
	let o = tpfs(&home, &["undo", "--approve", &ids[1]]);
	assert_eq!(code(&o), 1);
	assert!(String::from_utf8_lossy(&o.stderr).contains("trash was emptied"));
	assert!(stdout(&tpfs(&home, &["log"])).contains("trash emptied (cannot be undone)"));

	// By age: everything older than 0 days.
	let t = token(&tpfs(&home, &["trash", "empty", "--older-than", "0d"]));
	assert_eq!(code(&tpfs(&home, &["trash", "empty", "--older-than", "0d", "--approve", &t])), 0);
	assert!(stdout(&tpfs(&home, &["trash"])).contains("The trash is empty."));
}
