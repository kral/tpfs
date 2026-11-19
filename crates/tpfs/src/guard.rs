//! `tpfs guard`: a PreToolUse hook for AI coding agents (Claude Code, and Cursor, which runs a
//! project's Claude Code hooks). It reads the proposed tool call as JSON on stdin and refuses shell
//! commands that move, rename or remove files, pointing the agent to tpfs's MCP tools instead, so
//! file changes go through a reviewed, journaled, undoable plan.
//!
//! This is a guard rail, not a sandbox: it recognises the usual commands (also behind `sudo`,
//! `xargs`, `sh -c`, `find -exec` and one-line interpreter scripts), but a determined program can
//! still change files in ways no command line shows.

use std::{
	io::{self, Read},
	path::{Path, PathBuf},
};

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
	verdict_in(command, None)
}

/// As [`verdict`], and with the folder the command runs in (`cwd`), also refuse a command that
/// would overwrite an existing file: a `>` redirection to it, or a `cp` onto it.
pub fn verdict_in(command: &str, cwd: Option<&Path>) -> Option<String> {
	segments(command).into_iter().find_map(|words| judge(&words, 0, cwd))
}

/// Judge one simple command (its words).
fn judge(words: &[String], depth: usize, cwd: Option<&Path>) -> Option<String> {
	if depth > 4 {
		return None;
	}
	if let Some(cwd) = cwd
		&& let Some(why) = overwrites(words, cwd)
	{
		return Some(why);
	}
	let mut i = 0;
	// Leading variable assignments and wrappers (with their own options) run what follows.
	let mut after_wrapper = false;
	while let Some(w) = words.get(i) {
		if after_wrapper && w.starts_with('-') {
			// A wrapper's option; some take a value (`sudo -u bob`, `env -u NAME`, `xargs -n 1`).
			let with_value = matches!(
				w.as_str(),
				"-u" | "-g"
					| "-U" | "-C" | "-D"
					| "-h" | "-p" | "-r"
					| "-t" | "-n" | "-I"
					| "-L" | "-P" | "-s"
					| "-d" | "-E" | "-a"
			);
			i += if with_value { 2 } else { 1 };
		} else if WRAPPERS.contains(&base(w).as_str()) {
			after_wrapper = true;
			i += 1;
		} else if w.contains('=') && !w.starts_with('-') {
			i += 1;
		} else {
			break;
		}
	}
	let words = words.get(i..).unwrap_or_default();
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
			segments(&line).into_iter().find_map(|w| judge(&w, depth + 1, cwd))
		}
		"find" => {
			if words.iter().any(|w| w == "-delete") {
				return refuse("find -delete");
			}
			// `find … -exec rm {} ;`
			let exec = words.iter().position(|w| w == "-exec" || w == "-execdir" || w == "-ok")?;
			judge(&words[exec + 1..], depth + 1, cwd)
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

/// An existing file this command would replace: the target of a `>` redirection (not `>>`), or
/// what a copy lands on. A new file, a folder, and `/dev/null` (`NUL`) are fine.
fn overwrites(words: &[String], cwd: &Path) -> Option<String> {
	let existing_file = |target: &str| -> Option<PathBuf> {
		if matches!(target.to_ascii_lowercase().as_str(), "/dev/null" | "nul" | "/dev/stdout" | "/dev/stderr" | "$null")
		{
			return None;
		}
		let p = match target.strip_prefix("~/") {
			Some(rest) => {
				std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)?.join(rest)
			}
			None => cwd.join(target),
		};
		p.is_file().then_some(p)
	};
	let refuse = |p: &Path, how: &str| {
		Some(format!(
			"{how} would overwrite the existing file {}. Write to a new file, or edit it with your file-editing tool; \
			 to replace a file, move the old one aside with tpfs (propose_plan) first.",
			p.display()
		))
	};
	// Redirections: `>`, `1>`, `2>`, `&>`, `>|`, as a word of their own or glued to the target.
	for (i, w) in words.iter().enumerate() {
		let rest = w.trim_start_matches(|c: char| c.is_ascii_digit() || c == '&');
		let Some(after) = rest.strip_prefix('>') else { continue };
		if after.starts_with('>') || after.starts_with('&') {
			continue; // append, or a descriptor duplication
		}
		let after = after.strip_prefix('|').unwrap_or(after);
		let target = if after.is_empty() { words.get(i + 1).map(String::as_str) } else { Some(after) };
		if let Some(p) = target.and_then(existing_file) {
			return refuse(&p, "The `>` redirection");
		}
	}
	// A copy onto an existing file (or onto a file of the same name in an existing folder).
	let first = base(words.first()?);
	if matches!(first.as_str(), "cp" | "copy" | "copy-item" | "cpi" | "xcopy") {
		let args: Vec<&str> =
			words[1..].iter().map(String::as_str).filter(|a| !a.starts_with('-') && !a.starts_with('/')).collect();
		let (dest, sources) = args.split_last()?;
		if let Some(p) = existing_file(dest) {
			return refuse(&p, &format!("`{first}`"));
		}
		let dir = cwd.join(dest);
		if dir.is_dir() {
			for src in sources {
				let name = Path::new(src).file_name()?;
				if dir.join(name).is_file() {
					return refuse(&dir.join(name), &format!("`{first}`"));
				}
			}
		}
	}
	None
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
	// The folder the agent's shell is in (Claude Code sends it), for paths in the command.
	let cwd = call.get("cwd").and_then(|c| c.as_str()).map(PathBuf::from).or_else(|| std::env::current_dir().ok());
	let reason = verdict_in(command, cwd.as_deref())?;
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
			"sudo -u deploy rm -rf /srv/x",
			"env -u HOME rm x",
			"xargs -n 1 rm < list.txt",
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

	/// With the folder it runs in, a command that would overwrite an existing file is refused;
	/// a new file, an append and `/dev/null` are fine.
	#[test]
	fn overwriting_an_existing_file_is_refused() {
		let tmp = tempfile::TempDir::new().unwrap();
		let cwd = tmp.path();
		std::fs::write(cwd.join("notes.txt"), "mine").unwrap();
		std::fs::create_dir(cwd.join("dir")).unwrap();
		std::fs::write(cwd.join("dir").join("notes.txt"), "mine").unwrap();
		for cmd in [
			"echo x > notes.txt",
			"echo x >notes.txt",
			"cargo test 2> notes.txt",
			"cmd &> notes.txt",
			"cp a.txt notes.txt",
			"cp -f other/notes.txt dir",
			"Copy-Item x notes.txt",
		] {
			assert!(verdict_in(cmd, Some(cwd)).is_some(), "not refused: {cmd}");
		}
		for cmd in [
			"echo x > new.txt",
			"echo x >> notes.txt",
			"cargo test > /dev/null 2>&1",
			"cp notes.txt copy.txt",
			"cp -r src dir2",
			"ls 2>&1 | head",
		] {
			assert_eq!(verdict_in(cmd, Some(cwd)), None, "refused: {cmd}");
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
