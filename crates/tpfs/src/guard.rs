//! `tpfs guard`: a PreToolUse hook for AI coding agents (Claude Code, and Cursor, which runs a
//! project's Claude Code hooks). It reads the proposed tool call as JSON on stdin and refuses shell
//! commands that move, rename or remove files, pointing the agent to tpfs's MCP tools instead, so
//! file changes go through a reviewed, journaled, undoable plan.
//!
//! This is a guard rail, not a sandbox: it recognises the usual commands (also behind `sudo`,
//! `xargs`, `sh -c`, `find -exec` and one-line interpreter scripts), but a determined program can
//! still change files in ways no command line shows.

use std::io::{self, Read};

/// Commands that remove, move or rename files, as the first word of a command (Unix and Windows).
const MUTATING: &[&str] = &[
	"rm",
	"rmdir",
	"mv",
	"unlink",
	"shred",
	"srm",
	"truncate",
	"del",
	"erase",
	"rd",
	"move",
	"ren",
	"rename",
	"remove-item",
	"ri",
	"move-item",
	"mi",
	"rename-item",
	"rni",
	"clear-content",
];
/// Words that run the rest of the line as a command.
const WRAPPERS: &[&str] =
	&["sudo", "doas", "env", "nohup", "time", "command", "exec", "nice", "xargs", "busybox", "builtin"];
/// Interpreters whose one-line scripts are checked for file removal and renaming calls.
const INTERPRETERS: &[&str] = &["python", "python3", "python2", "node", "deno", "bun", "perl", "ruby", "php"];
/// Calls in such scripts that remove or rename files.
const SCRIPT_CALLS: &[&str] = &[
	"remove(",
	"unlink",
	"rmtree",
	"rmdir",
	"rename(",
	"replace(",
	"rmsync",
	"rm(",
	"renamesync",
	"move(",
	"fileutils.rm",
	"fileutils.mv",
	"file.delete",
	"delete_file",
];

/// Why a shell command is refused, or `None` when it does not move, rename or remove files.
pub fn verdict(command: &str) -> Option<String> {
	segments(command).into_iter().find_map(|words| judge(&words, 0))
}

/// Judge one simple command (its words).
fn judge(words: &[String], depth: usize) -> Option<String> {
	if depth > 4 {
		return None;
	}
	let mut i = 0;
	// Leading variable assignments and wrappers (with their own options) run what follows.
	while let Some(w) = words.get(i) {
		let name = base(w);
		if (w.contains('=') && !w.starts_with('-'))
			|| WRAPPERS.contains(&name.as_str())
			|| (i > 0 && w.starts_with('-') && WRAPPERS.contains(&base(&words[i - 1]).as_str()))
		{
			i += 1;
		} else {
			break;
		}
	}
	let words = &words[i..];
	let first = base(words.first()?);
	let refuse = |what: &str| {
		Some(format!(
			"`{what}` changes files outside tpfs. Move, rename or remove files with the tpfs MCP tools instead \
			 (propose_plan): the user reviews the plan, and it can be undone."
		))
	};
	if MUTATING.contains(&first.as_str()) {
		return refuse(&first);
	}
	match first.as_str() {
		// `sh -c '…'`, `bash -c "…"`, `cmd /c …`, `powershell -Command …`: the script is a command line.
		"sh" | "bash" | "zsh" | "dash" | "ksh" | "fish" | "cmd" | "cmd.exe" | "powershell" | "pwsh" => {
			let script = words
				.iter()
				.position(|w| matches!(w.to_ascii_lowercase().as_str(), "-c" | "/c" | "/k" | "-command" | "-c:"))?;
			let line = words[script + 1..].join(" ");
			segments(&line).into_iter().find_map(|w| judge(&w, depth + 1))
		}
		"find" => {
			if words.iter().any(|w| w == "-delete") {
				return refuse("find -delete");
			}
			// `find … -exec rm {} ;`
			let exec = words.iter().position(|w| w == "-exec" || w == "-execdir" || w == "-ok")?;
			judge(&words[exec + 1..], depth + 1)
		}
		"git" if words.get(1).is_some_and(|w| w == "clean") => refuse("git clean"),
		"rsync" if words.iter().any(|w| w.starts_with("--delete") || w == "--remove-source-files") => {
			refuse("rsync --delete")
		}
		i if INTERPRETERS.contains(&i.trim_end_matches(".exe")) => {
			let flag = words.iter().position(|w| matches!(w.as_str(), "-c" | "-e" | "--eval" | "-r"))?;
			let script = words.get(flag + 1)?.to_ascii_lowercase().replace(' ', "");
			SCRIPT_CALLS.iter().any(|c| script.contains(c)).then(|| refuse(&format!("{first} script")))?
		}
		_ => None,
	}
}

/// The command's name without a path or a Windows extension, lower case.
fn base(word: &str) -> String {
	let name = word.rsplit(['/', '\\']).next().unwrap_or(word).to_ascii_lowercase();
	name.strip_suffix(".exe").map(str::to_owned).unwrap_or(name)
}

/// Split a command line into simple commands (at `;`, `&`, `|`, newlines, `(`, `)`, backticks and
/// `$(`), each as a list of words with quotes removed. Quoted text stays one word; text inside
/// `$(…)` and backticks is a command of its own.
fn segments(line: &str) -> Vec<Vec<String>> {
	let mut out = Vec::new();
	let mut words: Vec<String> = Vec::new();
	let mut word = String::new();
	let mut quote: Option<char> = None;
	let mut quoted = false;
	let flush_word = |word: &mut String, words: &mut Vec<String>, quoted: &mut bool| {
		if !word.is_empty() || *quoted {
			words.push(std::mem::take(word));
		}
		*quoted = false;
	};
	let mut chars = line.chars().peekable();
	while let Some(c) = chars.next() {
		match (quote, c) {
			(Some(q), c) if c == q => quote = None,
			(Some('"'), '`') => {
				// A command substitution inside double quotes runs too.
				let inner: String = chars.by_ref().take_while(|&c| c != '`').collect();
				out.extend(segments(&inner));
			}
			(Some('"'), '$') if chars.peek() == Some(&'(') => {
				chars.next();
				let inner: String = chars.by_ref().take_while(|&c| c != ')').collect();
				out.extend(segments(&inner));
			}
			(Some(_), c) => word.push(c),
			(None, '\'' | '"') => {
				quote = Some(c);
				quoted = true;
			}
			(None, '\\') if chars.peek().is_some_and(|c| !c.is_alphanumeric()) => {
				// An escaped character (Unix); a Windows path keeps its backslashes.
				if let Some(n) = chars.next() {
					word.push(n);
				}
			}
			(None, c) if c.is_whitespace() && c != '\n' => flush_word(&mut word, &mut words, &mut quoted),
			(None, ';' | '&' | '|' | '\n' | '(' | ')' | '`' | '{' | '}') => {
				flush_word(&mut word, &mut words, &mut quoted);
				if !words.is_empty() {
					out.push(std::mem::take(&mut words));
				}
			}
			(None, '$') if chars.peek() == Some(&'(') => {
				flush_word(&mut word, &mut words, &mut quoted);
				if !words.is_empty() {
					out.push(std::mem::take(&mut words));
				}
			}
			(None, c) => word.push(c),
		}
	}
	flush_word(&mut word, &mut words, &mut quoted);
	if !words.is_empty() {
		out.push(words);
	}
	out
}

/// Run as a hook: the tool call on stdin, a refusal (Claude Code's PreToolUse JSON) on stdout.
/// Anything that is not a shell call it can read is let through: the hook guards, the client
/// decides the rest.
pub fn run_hook() -> io::Result<()> {
	let mut input = String::new();
	io::stdin().take(1 << 20).read_to_string(&mut input)?;
	if let Some(out) = respond(&input) {
		println!("{out}");
	}
	Ok(())
}

/// The hook's answer to one tool call (`None`: no objection).
pub fn respond(input: &str) -> Option<String> {
	let call: serde_json::Value = serde_json::from_str(input).ok()?;
	let tool = call.get("tool_name")?.as_str()?;
	if !matches!(tool, "Bash" | "Shell" | "PowerShell" | "run_terminal_cmd") {
		return None;
	}
	let command = call.get("tool_input")?.get("command")?.as_str()?;
	let reason = verdict(command)?;
	Some(
		serde_json::json!({
			"hookSpecificOutput": {
				"hookEventName": "PreToolUse",
				"permissionDecision": "deny",
				"permissionDecisionReason": reason,
			}
		})
		.to_string(),
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn file_changing_commands_are_refused() {
		for cmd in [
			"rm -rf tests/ patches/ plan/ ~/",
			"cd old-repo && rm -rf build",
			"sudo rm -r /var/www/old",
			"/bin/rm file",
			"mv a.py \"new folder\"",
			"ls | xargs rm",
			"find . -name '*.tmp' -delete",
			"find . -name '*.tmp' -exec rm {} \\;",
			"git clean -fdx",
			"rsync -a --delete src/ dst/",
			"bash -c 'rm -rf dist'",
			"sh -c \"cd x; mv a b\"",
			"echo $(rm -rf x)",
			"echo \"`rm x`\"",
			"FOO=1 rm x",
			"cmd /c \"rmdir /s /q d:\\Projects\\x\"",
			"del /s /q build",
			"Remove-Item -Recurse -Force .\\out",
			"pwsh -Command \"Move-Item a b\"",
			"python3 -c \"import shutil; shutil.rmtree('x')\"",
			"node -e \"require('fs').rmSync('x',{recursive:true})\"",
			"python -c 'import os; os.remove(\"a\")'",
			"rm.exe x",
		] {
			assert!(verdict(cmd).is_some(), "not refused: {cmd}");
		}
	}

	#[test]
	fn other_commands_pass() {
		for cmd in [
			"ls -la",
			"git status && git diff",
			"cargo test --workspace",
			"echo 'rm -rf /' > notes.txt",
			"grep -r \"mv \" src",
			"npm run build",
			"find . -name '*.rs'",
			"python3 -c 'print(1)'",
			"cat README.md | head",
			"git commit -m \"remove old files\"",
			"tpfs log",
		] {
			assert_eq!(verdict(cmd), None, "refused: {cmd}");
		}
	}

	#[test]
	fn the_hook_answers_in_claude_codes_format() {
		let out =
			respond(r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf ~/"}}"#)
				.unwrap();
		let v: serde_json::Value = serde_json::from_str(&out).unwrap();
		assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
		assert!(v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().contains("propose_plan"));
		assert_eq!(respond(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#), None);
		assert_eq!(respond(r#"{"tool_name":"Write","tool_input":{"file_path":"a"}}"#), None);
		assert_eq!(respond("not json"), None);
	}
}
