//! `tpfs doctor`: checks that an AI agent's setup really routes file changes through tpfs —
//! the MCP server is configured with human approval, the `tpfs guard` hook is in place, nothing
//! lets shell file commands run unchecked, and the state folder is private.

use std::{
	fs,
	path::{Path, PathBuf},
};

use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
	Ok,
	Warn,
	Fail,
}

pub struct Finding {
	pub level: Level,
	pub text: String,
}

fn ok(text: impl Into<String>) -> Finding {
	Finding { level: Level::Ok, text: text.into() }
}
fn warn(text: impl Into<String>) -> Finding {
	Finding { level: Level::Warn, text: text.into() }
}
fn fail(text: impl Into<String>) -> Finding {
	Finding { level: Level::Fail, text: text.into() }
}

/// The hook entry for Claude Code's settings (`.claude/settings.json`; Cursor runs it too).
pub const HOOK_SNIPPET: &str = r#"{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "tpfs guard" } ] }
    ]
  }
}"#;

fn read_json(path: &Path) -> Option<Value> {
	serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Every tpfs MCP server entry found: (where, its arguments).
fn tpfs_servers(home: &Path, project: &Path) -> Vec<(String, Vec<String>)> {
	let mut out = Vec::new();
	let mut look = |label: String, servers: Option<&Value>| {
		for (name, s) in servers.and_then(Value::as_object).into_iter().flatten() {
			let command = s.get("command").and_then(Value::as_str).unwrap_or_default();
			let args: Vec<String> = s
				.get("args")
				.and_then(Value::as_array)
				.into_iter()
				.flatten()
				.filter_map(|a| a.as_str().map(str::to_owned))
				.collect();
			let is_tpfs =
				Path::new(command).file_stem().is_some_and(|f| f == "tpfs") && args.first().is_some_and(|a| a == "mcp");
			if is_tpfs {
				out.push((format!("{label} ({name})"), args));
			}
		}
	};
	let mcp_json = project.join(".mcp.json");
	look(mcp_json.display().to_string(), read_json(&mcp_json).as_ref().and_then(|v| v.get("mcpServers")));
	let user = home.join(".claude.json");
	if let Some(v) = read_json(&user) {
		look(format!("{} (user)", user.display()), v.get("mcpServers"));
		let key = project.to_string_lossy();
		look(
			format!("{} (project {key})", user.display()),
			v.get("projects").and_then(|p| p.get(key.as_ref())).and_then(|p| p.get("mcpServers")),
		);
	}
	let cursor = home.join(".cursor").join("mcp.json");
	look(cursor.display().to_string(), read_json(&cursor).as_ref().and_then(|v| v.get("mcpServers")));
	let cursor_project = project.join(".cursor").join("mcp.json");
	look(cursor_project.display().to_string(), read_json(&cursor_project).as_ref().and_then(|v| v.get("mcpServers")));
	out
}

/// Claude Code settings files that apply to `project`, most general first.
fn settings_files(home: &Path, project: &Path) -> Vec<PathBuf> {
	vec![
		home.join(".claude").join("settings.json"),
		project.join(".claude").join("settings.json"),
		project.join(".claude").join("settings.local.json"),
	]
}

/// Whether a PreToolUse hook running `tpfs guard` covers the Bash tool in these settings.
fn has_guard(settings: &Value) -> bool {
	settings.get("hooks").and_then(|h| h.get("PreToolUse")).and_then(Value::as_array).into_iter().flatten().any(
		|entry| {
			let matcher = entry.get("matcher").and_then(Value::as_str).unwrap_or("");
			let covers_bash = matcher.is_empty() || matcher == "*" || matcher.split('|').any(|m| m.trim() == "Bash");
			covers_bash
				&& entry.get("hooks").and_then(Value::as_array).into_iter().flatten().any(|h| {
					h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("tpfs") && c.contains("guard"))
				})
		},
	)
}

/// Allow rules that let every shell command run without asking (with them, Claude Code has been
/// reported to ignore hook decisions).
fn blanket_bash(settings: &Value) -> Vec<String> {
	settings
		.get("permissions")
		.and_then(|p| p.get("allow"))
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter_map(Value::as_str)
		.filter(|r| matches!(*r, "Bash" | "Bash(*)" | "Bash(*:*)" | "Bash(:*)"))
		.map(str::to_owned)
		.collect()
}

/// Run every check for `project`, with `home` as the user's home and `state` as tpfs's state folder.
pub fn check(home: &Path, project: &Path, state: &Path) -> Vec<Finding> {
	let mut findings = Vec::new();

	let servers = tpfs_servers(home, project);
	if servers.is_empty() {
		findings.push(fail(format!(
			"no tpfs MCP server configured for {} — add one to .mcp.json: \
			 {{\"mcpServers\": {{\"tpfs\": {{\"command\": \"tpfs\", \"args\": [\"mcp\", \"--root\", \"<folder>\"]}}}}}}",
			project.display()
		)));
	}
	for (at, args) in &servers {
		let roots: Vec<&str> = args.windows(2).filter(|w| w[0] == "--root").map(|w| w[1].as_str()).collect();
		let client = args.windows(2).any(|w| w[0] == "--approval" && w[1] == "client");
		if client {
			findings.push(warn(format!(
				"{at}: --approval client — the MCP client's confirmation is the only approval; \"always allow\" there means none. Drop it to approve in a terminal."
			)));
		} else {
			findings.push(ok(format!("{at}: tpfs mcp, approval in a terminal")));
		}
		if roots.is_empty() {
			findings.push(fail(format!("{at}: no --root; the server will not start")));
		}
		for r in roots {
			let p = Path::new(r);
			if p == home || p.parent().is_none() {
				findings.push(warn(format!("{at}: root {r} is very broad; keep roots to the folders the task needs")));
			}
		}
	}

	let mut guarded = false;
	for file in settings_files(home, project) {
		let Some(settings) = read_json(&file) else { continue };
		if has_guard(&settings) {
			guarded = true;
			findings.push(ok(format!("{}: `tpfs guard` hook covers shell commands", file.display())));
		}
		for rule in blanket_bash(&settings) {
			findings.push(fail(format!(
				"{}: permissions.allow has \"{rule}\" — every shell command runs unasked, and hook decisions have been reported ignored with it. Allow specific commands instead.",
				file.display()
			)));
		}
	}
	if !guarded {
		findings.push(fail(format!(
			"no `tpfs guard` hook: the agent can still move and delete files with its shell. Add to {}:\n{HOOK_SNIPPET}",
			project.join(".claude").join("settings.json").display()
		)));
	}

	let cursor = home.join(".cursor").join("permissions.json");
	if let Some(v) = read_json(&cursor) {
		let risky: Vec<String> = v
			.get("terminalAllowlist")
			.and_then(Value::as_array)
			.into_iter()
			.flatten()
			.filter_map(Value::as_str)
			.filter(|c| {
				let first = c.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
				first == "*" || crate::guard::verdict(c).is_some() || crate::guard::verdict(&first).is_some()
			})
			.map(str::to_owned)
			.collect();
		if !risky.is_empty() {
			findings.push(fail(format!(
				"{}: terminalAllowlist auto-runs {} — remove them so they need approval",
				cursor.display(),
				risky.join(", ")
			)));
		}
	}

	match tpfs_core::private_dir(state) {
		Ok(()) => findings.push(ok(format!("state folder {} is private", state.display()))),
		Err(e) => findings.push(fail(e.to_string())),
	}
	findings
}

#[cfg(test)]
mod tests {
	use super::*;

	fn write(p: &Path, text: &str) {
		fs::create_dir_all(p.parent().unwrap()).unwrap();
		fs::write(p, text).unwrap();
	}

	#[test]
	fn a_complete_setup_passes_and_gaps_are_named() {
		let tmp = tempfile::TempDir::new().unwrap();
		let (home, project) = (tmp.path().join("home"), tmp.path().join("home").join("proj"));
		let state = tmp.path().join("state");
		fs::create_dir_all(&project).unwrap();

		let levels = |f: &[Finding]| f.iter().filter(|x| x.level == Level::Fail).count();
		let empty = check(&home, &project, &state);
		assert_eq!(levels(&empty), 2, "no server, no hook");

		write(
			&project.join(".mcp.json"),
			r#"{"mcpServers":{"tpfs":{"command":"/usr/local/bin/tpfs","args":["mcp","--root","/srv/x"]}}}"#,
		);
		write(&project.join(".claude/settings.json"), HOOK_SNIPPET);
		let good = check(&home, &project, &state);
		assert_eq!(levels(&good), 0, "{:?}", good.iter().map(|f| &f.text).collect::<Vec<_>>());

		write(&home.join(".claude/settings.json"), r#"{"permissions":{"allow":["Bash"]}}"#);
		write(&home.join(".cursor/permissions.json"), r#"{"terminalAllowlist":["ls","rm","git status"]}"#);
		write(
			&project.join(".mcp.json"),
			r#"{"mcpServers":{"t":{"command":"tpfs","args":["mcp","--root","/srv/x","--approval","client"]}}}"#,
		);
		let bad = check(&home, &project, &state);
		let text: Vec<&str> = bad.iter().map(|f| f.text.as_str()).collect();
		assert_eq!(levels(&bad), 2, "{text:?}");
		assert!(text.iter().any(|t| t.contains("\"Bash\"")) && text.iter().any(|t| t.contains("auto-runs rm")));
		assert!(bad.iter().any(|f| f.level == Level::Warn && f.text.contains("--approval client")));
	}
}
