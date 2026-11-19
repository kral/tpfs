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
