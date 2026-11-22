mod doctor;
mod guard;
mod recipe_cli;

use std::{
	ffi::OsString,
	fs,
	io::{self, BufRead, IsTerminal, Read, Write},
	path::{Path, PathBuf},
	process::ExitCode,
};

use tpfs_core::{
	BatchStatus, ExecError, Executor, Journal, Plan, Source, Validator,
	risk::{self, Level},
};

const USAGE: &str = "\
tpfs — plan, preview, apply and undo file operations safely

USAGE:
  tpfs [ui] [<left dir>] [<right dir>] [--icons]
                                                  two-panel file manager (Norton Commander style);
                                                  the default when started in a terminal.
                                                  --icons (or TPFS_ICONS=1) needs a Nerd Font
  tpfs schema                                   print the JSON Schema of a plan
  tpfs check  <plan.json|-> --root <dir>...     validate; show preview, risk and digest
                [--source human|agent]            judge the risk as for this proposer
                [--max-ops <n>]                   allow a longer human plan (default 10000)
  tpfs apply  <plan.json|-> --root <dir>...     validate, confirm, execute
                [--approve <digest>]              non-interactive approval of exactly this plan
                [--source human|agent]            who proposed the plan (default: human)
                [--max-ops <n>]                   allow a longer human plan (default 10000)
  tpfs sync <source> <target>                   compare two folders and show the sync plan
                [--both]                          copy both ways, the newer file wins
                [--mirror]                        one-way: move target-only items to the trash
                [--apply]                         confirm and execute (or --approve <digest>)
                [--json]                          print the plan as JSON (for `tpfs apply -`)
  tpfs undo [--approve <batch>]                 revert the most recent batch
  tpfs log [--all] [<batch>]                    list batches (--all: with the archived history), or
                                                every step of one batch and its undo
  tpfs recover                                  roll back batches interrupted by a crash
                [--abandon <batch>]               give up on one recovery cannot handle (damaged, or
                                                  older than newer changes): records it as abandoned
                                                  and changes nothing on disk; see `tpfs log` first
  tpfs trash                                    list what the trash holds per batch: date, source,
                                                  items, size, and whether undo can still use it
  tpfs trash empty                              delete for good what batches left in the trash (the
                                                  only permanent delete); asks for a typed yes
                [--older-than <N>d]               batches older than N days (default: 30d)
                [--batch <batch>]                 only this batch (with --older-than: if old enough)
                [--approve <token>]               non-interactive: exactly the selection shown
                                                  with this token; those batches cannot be undone

  tpfs mcp --root <dir>... [--approval terminal|client]
                                                  MCP server on stdio for AI agents (agents propose,
                                                  humans approve; default approval: terminal)
  tpfs pending                                  list plans proposed by agents
  tpfs approve <planId>                         review and approve an agent's plan (interactive)
  tpfs reject <planId>                          discard an agent's plan
  tpfs watch                                    review agents' plans (and recipes) as they arrive:
                                                  approve (yes), reject (no) or leave for later (Enter)
  tpfs recipes                                  list recipes agents proposed, and installed ones
  tpfs recipe install <id> [--root <dir>...]    review a proposed recipe and install it (interactive);
                                                  --root narrows the folders it may work in
  tpfs recipe reject <id>                       discard a proposed recipe
  tpfs recipe show <id>                         show a proposed or installed recipe
  tpfs recipe run <recipe> <entry>... [--set <field>=<value>]...
                                                  expand an installed recipe over the entries, show
                                                  the plan, confirm and execute it (or --approve <digest>;
                                                  a recipe's first 3 runs need a typed yes in a terminal)
  tpfs recipe remove <recipe>                   uninstall a recipe
  tpfs guard                                    hook for AI agents (Claude Code, Cursor): refuses shell
                                                  commands that move, rename or delete files and points
                                                  the agent to tpfs (reads the tool call on stdin)
  tpfs doctor [<project>]                       check an AI agent setup: tpfs MCP server, terminal
                                                  approval, `tpfs guard` hook, no blanket shell allow

OPTIONS:
  --home <dir>    state directory (journal, trash). Default: $TPFS_HOME, else
                  $XDG_STATE_HOME/tpfs or ~/.local/state/tpfs (Unix), %LOCALAPPDATA%\\tpfs (Windows)
  -h, --help      show this help (`tpfs <command> --help` for one command)
  -V, --version   print the version

EXIT CODES:
  0  done (also when there was nothing to do)    3  plan rejected by validation
  1  failed                                      4  cancelled or not approved; nothing changed
  2  usage error

No plan ever deletes anything: `trash` moves into <home>/trash (on Linux and macOS, items on
another disk go to a private .tpfs-trash-<uid> folder at that disk's top) and can be undone. Only
`tpfs trash empty`, run by a person, deletes from the trash; agents cannot (not over MCP, and
`tpfs guard` refuses it).
With --approval client, the MCP client's own confirmation is the only approval: a client set to
\"always allow\" means no human approval at all.
";

/// Failure with its exit code (see EXIT CODES in the usage).
struct Fail(u8, String);

impl From<String> for Fail {
	fn from(s: String) -> Self {
		Fail(1, s)
	}
}

impl From<&str> for Fail {
	fn from(s: &str) -> Self {
		Fail(1, s.to_owned())
	}
}

fn usage(msg: impl Into<String>) -> Fail {
	Fail(2, msg.into())
}

fn cancelled(msg: impl Into<String>) -> Fail {
	Fail(4, msg.into())
}

type Res = Result<ExitCode, Fail>;

/// The part of the usage about one command.
fn command_help(command: &str) -> Option<String> {
	let start = format!("  tpfs {command}");
	let mut out = String::new();
	let mut inside = false;
	for line in USAGE.lines() {
		if line.starts_with("  tpfs ") {
			let ui = command == "ui" && line.starts_with("  tpfs [ui]");
			inside = ui || (line.starts_with(&start) && line[start.len()..].chars().next().is_none_or(|c| c == ' '));
		} else if !line.starts_with("    ") {
			inside = false;
		}
		if inside {
			out.push_str(line);
			out.push('\n');
		}
	}
	(!out.is_empty()).then(|| format!("USAGE:\n{out}\nSee `tpfs --help` for options and exit codes.\n"))
}

/// Flags each command accepts, and how many positional arguments.
fn allowed(command: &str) -> Option<(&'static [&'static str], usize)> {
	Some(match command {
		"ui" => (&["icons", "home"], 2),
		"schema" => (&[], 0),
		"check" => (&["root", "source", "max-ops", "home"], 1),
		"apply" => (&["root", "approve", "source", "max-ops", "home"], 1),
		"sync" => (&["both", "mirror", "apply", "approve", "json", "home"], 2),
		"undo" => (&["approve", "home"], 0),
		"log" => (&["home", "all"], 1),
		"pending" => (&["home"], 0),
		"recover" => (&["home", "abandon"], 0),
		"trash" => (&["home", "older-than", "batch", "approve"], 1),
		"mcp" => (&["root", "approval", "home"], 0),
		"approve" | "reject" => (&["home"], 1),
		"guard" => (&[], 0),
		"watch" => (&["home"], 0),
		"recipes" => (&["home"], 0),
		"recipe" => (&["home", "root", "set", "approve"], usize::MAX),
		"doctor" => (&["home"], 1),
		_ => return None,
	})
}

struct Args {
	command: String,
	plan: Option<OsString>,
	extra: Option<OsString>,
	/// Positional arguments after the first two (`tpfs recipe run <id> <entry>…`).
	rest: Vec<OsString>,
	/// `--set field=value`, in order.
	sets: Vec<String>,
	roots: Vec<PathBuf>,
	approve: Option<String>,
	approval: tpfs_mcp::Approval,
	source: Source,
	home: Option<PathBuf>,
	icons: bool,
	both: bool,
	/// `tpfs log --all`: the archived history too.
	all: bool,
	mirror: bool,
	do_apply: bool,
	json: bool,
	max_ops: Option<usize>,
	/// Flags given, to reject ones the command does not use.
	given: Vec<&'static str>,
	positional: usize,
	help: bool,
	abandon: Option<String>,
	/// `tpfs trash empty --older-than <N>d`, in days.
	older_than: Option<u64>,
	/// `tpfs trash empty --batch <id>`.
	batch: Option<String>,
}

fn main() -> ExitCode {
	// `tpfs log | head`: end quietly when the reader goes away, like other command-line tools,
	// instead of panicking on a broken pipe.
	#[cfg(unix)]
	// SAFETY: restoring the default action for SIGPIPE before any thread exists.
	unsafe {
		libc::signal(libc::SIGPIPE, libc::SIG_DFL);
	}
	let args = match parse_args() {
		Ok(a) => a,
		Err(Fail(code, e)) => {
			eprintln!("tpfs: {e}");
			return ExitCode::from(code);
		}
	};
	let command = args.command.clone();
	match run(args) {
		Ok(code) => code,
		Err(Fail(code, e)) => {
			eprintln!("tpfs {command}: {e}");
			ExitCode::from(code)
		}
	}
}

fn parse_args() -> Result<Args, Fail> {
	use lexopt::prelude::*;

	let mut parser = lexopt::Parser::from_env();
	let mut args = Args {
		command: String::new(),
		plan: None,
		extra: None,
		rest: vec![],
		sets: vec![],
		roots: vec![],
		approve: None,
		approval: tpfs_mcp::Approval::Terminal,
		source: Source::Human,
		home: None,
		icons: false,
		both: false,
		all: false,
		mirror: false,
		do_apply: false,
		json: false,
		max_ops: None,
		given: vec![],
		positional: 0,
		help: false,
		abandon: None,
		older_than: None,
		batch: None,
	};

	let e = |e: lexopt::Error| usage(e.to_string());
	while let Some(arg) = parser.next().map_err(e)? {
		if let Long(name) = &arg {
			const FLAGS: [&str; 15] = [
				"root",
				"approve",
				"approval",
				"icons",
				"both",
				"mirror",
				"apply",
				"json",
				"max-ops",
				"home",
				"source",
				"all",
				"set",
				"older-than",
				"batch",
			];
			if let Some(f) = FLAGS.iter().find(|f| **f == *name) {
				args.given.push(f);
			}
		}
		if matches!(arg, Value(_)) && !args.command.is_empty() {
			args.positional += 1;
		}
		match arg {
			Short('h') | Long("help") => args.help = true,
			Short('V') | Long("version") => {
				println!("tpfs {}", env!("CARGO_PKG_VERSION"));
				std::process::exit(0);
			}
			Long("root") => args.roots.push(parser.value().map_err(e)?.into()),
			Long("approve") => {
				args.approve = Some(parser.value().map_err(e)?.string().map_err(|e| usage(e.to_string()))?)
			}
			Long("approval") => {
				args.approval = match parser.value().map_err(e)?.string().map_err(|e| usage(e.to_string()))?.as_str() {
					"terminal" => tpfs_mcp::Approval::Terminal,
					"client" => tpfs_mcp::Approval::Client,
					other => return Err(usage(format!("unknown approval mode '{other}' (terminal|client)"))),
				}
			}
			Long("abandon") => {
				args.abandon = Some(parser.value().map_err(e)?.string().map_err(|e| usage(e.to_string()))?)
			}
			Long("older-than") => {
				let v = parser.value().map_err(e)?.string().map_err(|e| usage(e.to_string()))?;
				let days = v.strip_suffix('d').unwrap_or(&v).parse();
				args.older_than =
					Some(days.map_err(|_| usage(format!("--older-than needs days, like 30d; got '{v}'")))?);
			}
			Long("batch") => args.batch = Some(parser.value().map_err(e)?.string().map_err(|e| usage(e.to_string()))?),
			Long("set") => args.sets.push(parser.value().map_err(e)?.string().map_err(|e| usage(e.to_string()))?),
			Long("icons") => args.icons = true,
			Long("both") => args.both = true,
			Long("all") => args.all = true,
			Long("mirror") => args.mirror = true,
			Long("apply") => args.do_apply = true,
			Long("json") => args.json = true,
			Long("max-ops") => {
				let v = parser.value().map_err(e)?.string().map_err(|e| usage(e.to_string()))?;
				args.max_ops = Some(v.parse().map_err(|_| usage(format!("--max-ops needs a number, got '{v}'")))?);
			}
			Long("home") => args.home = Some(parser.value().map_err(e)?.into()),
			Long("source") => {
				args.source = match parser.value().map_err(e)?.string().map_err(|e| usage(e.to_string()))?.as_str() {
					"human" => Source::Human,
					"agent" => Source::Agent,
					other => return Err(usage(format!("unknown source '{other}' (human|agent)"))),
				}
			}
			Value(v) if args.command.is_empty() => args.command = v.string().map_err(|e| usage(e.to_string()))?,
			Value(v) if args.plan.is_none() => args.plan = Some(v),
			Value(v) if args.extra.is_none() => args.extra = Some(v),
			Value(v) => args.rest.push(v),
			other => return Err(usage(other.unexpected().to_string())),
		}
	}

	// `tpfs <dir> [<dir>]` opens the commander there.
	const COMMANDS: [&str; 15] = [
		"ui", "schema", "check", "apply", "sync", "undo", "log", "recover", "mcp", "pending", "approve", "reject",
		"recipes", "recipe", "trash",
	];
	if !args.command.is_empty() && !COMMANDS.contains(&args.command.as_str()) && Path::new(&args.command).is_dir() {
		args.extra = args.plan.take();
		args.plan = Some(std::mem::take(&mut args.command).into());
		args.command = "ui".into();
	}
	if args.help {
		match command_help(&args.command) {
			Some(h) if !args.command.is_empty() => print!("{h}"),
			_ => print!("{USAGE}"),
		}
		std::process::exit(0);
	}
	if args.command.is_empty() {
		if io::stdin().is_terminal() && io::stdout().is_terminal() {
			args.command = "ui".into();
		} else {
			return Err(usage("missing command (see `tpfs --help`)"));
		}
	}
	let Some((flags, max_positional)) = allowed(&args.command) else {
		return Err(usage(format!("'{}' is neither a command nor a folder (see `tpfs --help`)", args.command)));
	};
	if let Some(f) = args.given.iter().find(|f| !flags.contains(f)) {
		return Err(usage(format!("{}: --{f} does not apply here (see `tpfs {} --help`)", args.command, args.command)));
	}
	if args.positional > max_positional {
		return Err(usage(format!("{}: too many arguments (see `tpfs {} --help`)", args.command, args.command)));
	}
	Ok(args)
}

fn run(args: Args) -> Res {
	match args.command.as_str() {
		"schema" => {
			println!("{}", serde_json::to_string_pretty(&Plan::schema()).map_err(|e| e.to_string())?);
			Ok(ExitCode::SUCCESS)
		}
		"guard" => {
			guard::run_hook().map_err(|e| e.to_string())?;
			Ok(ExitCode::SUCCESS)
		}
		"doctor" => {
			let project = match &args.plan {
				Some(p) => PathBuf::from(p),
				None => std::env::current_dir().map_err(|e| e.to_string())?,
			};
			let project = std::path::absolute(&project).map_err(|e| e.to_string())?;
			let home = std::env::var_os("HOME")
				.or_else(|| std::env::var_os("USERPROFILE"))
				.map(PathBuf::from)
				.ok_or("cannot tell the home folder (HOME or USERPROFILE)")?;
			let findings = doctor::check(&home, &project, &state_dir(&args)?);
			for f in &findings {
				let mark = match f.level {
					doctor::Level::Ok => "ok  ",
					doctor::Level::Warn => "warn",
					doctor::Level::Fail => "FAIL",
				};
				for (i, line) in f.text.lines().enumerate() {
					let line = tpfs_core::sanitize(line.as_ref());
					if i == 0 { println!("{mark}  {line}") } else { println!("      {line}") }
				}
			}
			let failed = findings.iter().filter(|f| f.level == doctor::Level::Fail).count();
			if failed > 0 {
				println!(
					"\n{failed} problem(s): the agent can still change files outside tpfs or without your approval."
				);
				return Ok(ExitCode::from(1));
			}
			println!("\nThe agent's file changes go through tpfs and need your approval.");
			Ok(ExitCode::SUCCESS)
		}
		"check" => check(&args, false),
		"apply" => check(&args, true),
		"sync" => sync(&args),
		"trash" => trash(&args),
		"undo" => {
			let (mut journal, _) = open_state(&args)?;
			let validator = any_validator(&args, &mut journal)?;
			let batches = journal.batches().map_err(|e| e.to_string())?;
			// The same choice as the engine: a newer interrupted batch must be recovered first, so
			// do not offer an older one only to refuse it after the user said yes.
			if let Some(b) = batches
				.iter()
				.rev()
				.find(|b| !matches!(b.status, BatchStatus::Aborted | BatchStatus::Undone | BatchStatus::Abandoned))
				&& b.status == BatchStatus::Incomplete
			{
				let id = tpfs_core::sanitize(b.id.as_ref());
				return Err(if b.damaged {
					format!("batch {id} is interrupted and its journal records are damaged; see `tpfs log`, then `tpfs recover --abandon {id}` if you accept the disk as it is").into()
				} else {
					format!("batch {id} was interrupted; run `tpfs recover` first (it rolls it back), then undo").into()
				});
			}
			let last = batches.into_iter().rev().find(|b| b.status == BatchStatus::Committed);
			if let Some(b) = last.as_ref().filter(|b| b.damaged) {
				return Err(format!(
					"batch {id} has damaged journal records; it cannot be undone automatically. See its steps with `tpfs log {id}`, undo by hand what you need, then `tpfs recover --abandon {id}` to move past it",
					id = tpfs_core::sanitize(b.id.as_ref())
				)
				.into());
			}
			if let Some(b) = last.as_ref().filter(|b| b.purged) {
				return Err(format!("batch {}: {}", tpfs_core::sanitize(b.id.as_ref()), tpfs_core::PURGED).into());
			}
			// Not held while the user reads; the batch is checked again when undoing.
			drop(journal);
			let Some(b) = &last else {
				println!("Nothing to undo.");
				return Ok(ExitCode::SUCCESS);
			};
			{
				let steps: Vec<_> = b.steps.iter().filter(|s| s.done && !s.undone).collect();
				if let Some(approved) = &args.approve {
					// Non-interactive: name the batch, as `apply` names the plan by its digest.
					if *approved != b.id {
						return Err(cancelled(format!(
							"--approve {} does not name the newest batch ({}); nothing was changed",
							tpfs_core::sanitize(approved.as_ref()),
							tpfs_core::sanitize(b.id.as_ref())
						)));
					}
				} else if io::stdin().is_terminal() && io::stderr().is_terminal() {
					eprintln!(
						"Undo batch {} by {} — {} step(s): {}",
						tpfs_core::sanitize(b.id.as_ref()),
						if b.source == "agent" { "an AI agent" } else { "a person" },
						steps.len(),
						tpfs_core::sanitize(b.reason.as_ref())
					);
					eprintln!("These changes will be reversed:");
					for s in steps.iter().take(20) {
						eprintln!("  {}", s.forward.normalized());
					}
					if steps.len() > 20 {
						eprintln!("  … and {} more (see `tpfs log`)", steps.len() - 20);
					}
					if !matches!(ask("Undo it? [y/N] ")?.as_str(), "y" | "Y" | "yes") {
						return Err(cancelled("cancelled; nothing was changed"));
					}
				} else {
					return Err(cancelled(format!(
						"not interactive: pass --approve {} to undo exactly that batch (see `tpfs log`)",
						tpfs_core::sanitize(b.id.as_ref())
					)));
				}
			}
			let (mut journal, trash) = open_state(&args)?;
			let mut executor = Executor::new(&validator, &mut journal, trash);
			// Exactly the batch shown and approved: another process may have committed one since.
			let result = executor.undo_batch(&b.id);
			match result {
				Err(ExecError::Undo { error, .. }) if error.contains("no longer the newest batch") => {
					return Err(cancelled(format!(
						"the history changed meanwhile: {}; run `tpfs undo` again",
						tpfs_core::sanitize(error.as_ref())
					)));
				}
				Ok(Some(b)) => {
					println!("Undone batch {} ({} step(s)):", tpfs_core::sanitize(b.id.as_ref()), b.steps);
					for s in last.iter().flat_map(|l| l.steps.iter().filter(|s| s.done && !s.undone)) {
						println!("  reverted: {}", s.forward.normalized());
					}
				}
				Ok(None) => println!("Nothing to undo."),
				Err(e) => return Err(e.to_string().into()),
			}
			Ok(ExitCode::SUCCESS)
		}
		"log" => {
			let (mut journal, _) = open_state(&args)?;
			eprintln!("State: {}", tpfs_core::sanitize(state_dir(&args)?.as_os_str()));
			let mut batches = journal.batches().map_err(|e| e.to_string())?;
			// Older history, moved out of the journal once it grew (see `Journal::compact`).
			let archived = if args.all || args.plan.is_some() {
				journal.archived().map_err(|e| e.to_string())?
			} else {
				Vec::new()
			};
			let in_archive = fs::metadata(journal.archive_path()).is_ok_and(|m| m.len() > 0);
			batches.splice(0..0, archived);
			// `tpfs log <batch>`: every step of one batch, what it did and how it is undone.
			if let Some(raw) = args.plan.as_deref() {
				let id = raw
					.to_str()
					.ok_or_else(|| usage(format!("no batch {} in the journal", tpfs_core::sanitize(raw))))?;
				let b = batches
					.iter()
					.find(|b| b.id == id)
					.ok_or_else(|| usage(format!("no batch {} in the journal", tpfs_core::sanitize(id.as_ref()))))?;
				println!(
					"Batch {} by {}{}{}: {}",
					tpfs_core::sanitize(b.id.as_ref()),
					tpfs_core::sanitize(b.source.as_ref()),
					if b.damaged { " (journal records damaged: the list may be incomplete)" } else { "" },
					if b.purged { " (its trash was emptied: it cannot be undone)" } else { "" },
					tpfs_core::sanitize(b.reason.as_ref())
				);
				for st in &b.steps {
					let state = if st.undone {
						"undone"
					} else if st.done {
						"done"
					} else if st.failed {
						"failed"
					} else {
						"started"
					};
					println!("  #{:<4} {state:<8} {}", st.index + 1, st.forward.normalized());
					println!("         undo:    {}", st.inverse.normalized());
				}
				return Ok(ExitCode::SUCCESS);
			}
			if batches.is_empty() {
				println!("No batches recorded.");
			}
			for b in &batches {
				let status = match b.status {
					BatchStatus::Committed => "committed",
					BatchStatus::Aborted => "rolled back",
					BatchStatus::Undone => "undone",
					BatchStatus::Incomplete => "INCOMPLETE (run `tpfs recover`)",
					BatchStatus::Abandoned => "abandoned (changes left on disk)",
					_ => "unknown",
				};
				let status = match b.status {
					BatchStatus::Abandoned | BatchStatus::Undone | BatchStatus::Aborted => status,
					_ if b.damaged => "DAMAGED (see `tpfs log <batch>`)",
					BatchStatus::Committed if b.purged => "committed, trash emptied (cannot be undone)",
					_ => status,
				};
				println!(
					"{}  {:<6} {:<10} {} step(s)  {}",
					tpfs_core::sanitize(b.id.as_ref()),
					tpfs_core::sanitize(b.source.as_ref()),
					status,
					b.steps.len(),
					tpfs_core::sanitize(b.reason.as_ref())
				);
			}
			if journal.corrupt_lines() > 0 {
				eprintln!("warning: {} unreadable journal line(s) skipped", journal.corrupt_lines());
			}
			if in_archive && !args.all {
				eprintln!("Older, finished batches are archived: `tpfs log --all` lists them too.");
			}
			Ok(ExitCode::SUCCESS)
		}
		"recover" => {
			let (mut journal, trash) = open_state(&args)?;
			let validator = any_validator(&args, &mut journal)?;
			if let Some(id) = &args.abandon {
				let shown = tpfs_core::sanitize(id.as_ref());
				match journal.batches().map_err(|e| e.to_string())?.iter().find(|b| b.id == *id) {
					None => return Err(usage(format!("no batch {shown} in the journal (see `tpfs log`)"))),
					Some(b) if b.status == BatchStatus::Abandoned => {
						return Err(usage(format!("batch {shown} is already abandoned; nothing to do")));
					}
					Some(b)
						if !(b.status == BatchStatus::Incomplete
							|| (b.damaged && b.status == BatchStatus::Committed)) =>
					{
						return Err(usage(format!(
							"batch {shown} is neither interrupted nor damaged; nothing to abandon"
						)));
					}
					_ => {}
				}
				if io::stdin().is_terminal() && io::stderr().is_terminal() {
					drop(journal); // not held while the user reads
					let answer = ask(&format!("Abandon batch {shown}? Its changes stay on disk as they are. [y/N] "))?;
					if !matches!(answer.as_str(), "y" | "Y" | "yes") {
						return Err(cancelled("cancelled; nothing was changed"));
					}
					journal = open_state(&args)?.0;
				}
				Executor::new(&validator, &mut journal, trash).abandon(id).map_err(|e| match e {
					ExecError::Blocked(m) if m.starts_with("no batch") => usage(format!("{m} (see `tpfs log`)")),
					e => e.to_string().into(),
				})?;
				println!("Abandoned batch {shown}; its changes were left as they are.");
				return Ok(ExitCode::SUCCESS);
			}
			let report = Executor::new(&validator, &mut journal, &trash).recover().map_err(|e| e.to_string())?;
			// An emptying of the trash cut short: already recorded, so nothing claims to be undoable;
			// the rest is deleted by the next `tpfs trash empty`.
			let cut_short =
				tpfs_core::trash::contents(&mut journal, &trash).map_or(0, |c| c.iter().filter(|t| t.purged).count());
			if cut_short > 0 {
				eprintln!(
					"Emptying the trash of {cut_short} batch(es) was cut short; they cannot be undone. `tpfs trash empty` finishes it."
				);
			}
			match (report.rolled_back.len(), report.errors.is_empty()) {
				(0, true) => println!("Nothing to recover."),
				(0, false) => println!("Nothing rolled back."),
				(n, _) => println!("Rolled back {n} interrupted batch(es)."),
			}
			for e in &report.errors {
				eprintln!("  error: {e}");
			}
			// Batches recover can retry (as opposed to those it only reports, which say what to do).
			if report.errors.iter().any(|e| !e.contains("--abandon")) {
				eprintln!("Fix the cause of each error, then run `tpfs recover` again (finished steps are skipped).");
			}
			Ok(if report.errors.is_empty() { ExitCode::SUCCESS } else { ExitCode::from(1) })
		}
		"ui" => {
			let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
			let left = args.plan.clone().map(PathBuf::from).unwrap_or_else(|| cwd.clone());
			let right = args.extra.clone().map(PathBuf::from).unwrap_or(cwd);
			let icons = args.icons || std::env::var_os("TPFS_ICONS").is_some_and(|v| v != "0");
			tpfs_tui::run(left, right, state_dir(&args)?, tpfs_tui::Options { icons }).map_err(|e| e.to_string())?;
			Ok(ExitCode::SUCCESS)
		}
		"mcp" => {
			if args.roots.is_empty() {
				return Err(usage("at least one --root <dir> is required"));
			}
			let config = tpfs_mcp::Config {
				roots: args.roots.clone(),
				// Absolute: the agent's client may start the server anywhere, and the human
				// runs the hinted `tpfs approve --home …` from somewhere else again.
				state: std::path::absolute(state_dir(&args)?).map_err(|e| e.to_string())?,
				approval: args.approval,
			};
			// Checked now, not after the human approved a plan that could then not be applied.
			tpfs_core::private_dir(&config.state).map_err(|e| e.to_string())?;
			let mut server = tpfs_mcp::Server::new(config).map_err(|e| format!("invalid root: {e}"))?;
			eprintln!("tpfs mcp: serving on stdio");
			server.run(io::stdin().lock(), io::stdout().lock()).map_err(|e| e.to_string())?;
			Ok(ExitCode::SUCCESS)
		}
		"pending" => {
			let store = tpfs_mcp::Store::new(&state_dir(&args)?);
			let (pending, damaged) = store.scan().map_err(|e| e.to_string())?;
			if pending.is_empty() && damaged.is_empty() {
				println!("No pending plans.");
			}
			for id in &damaged {
				println!("{id}  DAMAGED  (unreadable; remove it with `tpfs reject {id}`)");
			}
			for p in &pending {
				let approved = if store.is_claimed(&p.id) {
					"used"
				} else if store.is_approved(p) {
					"approved"
				} else {
					"waiting"
				};
				println!(
					"{}  {:<8} {} op(s)  {}",
					tpfs_core::sanitize(p.id.as_ref()),
					approved,
					p.plan.ops.len(),
					tpfs_core::sanitize(p.plan.reason.as_ref())
				);
			}
			let recipes = tpfs_mcp::RecipeStore::new(&state_dir(&args)?).scan().map_or(0, |(r, d)| r.len() + d.len());
			if recipes > 0 {
				println!("{recipes} recipe(s) proposed by agents are waiting too: see `tpfs recipes`.");
			}
			Ok(ExitCode::SUCCESS)
		}
		"approve" => approve(&args),
		"watch" => watch(&args),
		"recipes" => recipe_cli::list(&args),
		"recipe" => recipe_cli::run(&args),
		"reject" => {
			let id = plan_id(&args)?;
			let store = tpfs_mcp::Store::new(&state_dir(&args)?);
			// A damaged pending file can be rejected too (only a missing one or a bad id is an error).
			if let Err(e) = store.get(&id)
				&& matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidInput)
			{
				return Err(usage(format!("{e} (see `tpfs pending`)")));
			}
			store.reject(&id).map_err(|e| e.to_string())?;
			println!("Rejected {}.", tpfs_core::sanitize(id.as_ref()));
			Ok(ExitCode::SUCCESS)
		}
		other => Err(usage(format!("unknown command '{other}' (see `tpfs --help`)"))),
	}
}

/// `tpfs trash`: what the trash holds per batch. `tpfs trash empty`: delete some of it for good,
/// after a typed `yes` (or `--approve <token>` naming exactly the selection shown).
fn trash(args: &Args) -> Res {
	use tpfs_core::trash::{self, Trashed};
	let sub = args.plan.as_deref().map(|s| s.to_str().unwrap_or("?"));
	if sub.is_none() && (args.older_than.is_some() || args.batch.is_some() || args.approve.is_some()) {
		return Err(usage("--older-than, --batch and --approve go with `tpfs trash empty`"));
	}
	if let Some(other) = sub.filter(|s| *s != "empty") {
		return Err(usage(format!(
			"unknown trash command '{}' (see `tpfs trash --help`)",
			tpfs_core::sanitize(other.as_ref())
		)));
	}
	let (mut journal, trash_dir) = open_state(args)?;
	let all = trash::contents(&mut journal, &trash_dir).map_err(|e| e.to_string())?;
	let line = |t: &Trashed| {
		format!(
			"{:<22} {}  {:<6} {:>6}  {:>10}  {}",
			tpfs_core::sanitize(t.id.as_ref()),
			date(t.at),
			if t.source.is_empty() { "?".into() } else { tpfs_core::sanitize(t.source.as_ref()) },
			t.items,
			size(t.bytes),
			t.undo_state()
		)
	};
	let header =
		format!("{:<22} {:<16}  {:<6} {:>6}  {:>10}  UNDOABLE", "BATCH", "DATE (UTC)", "SOURCE", "ITEMS", "SIZE");
	let totals = |ts: &[Trashed]| {
		let (items, bytes) = ts.iter().fold((0, 0), |(i, b), t| (i + t.items, b + t.bytes));
		format!("{} batch(es), {items} item(s), {}", ts.len(), size(bytes))
	};

	if sub.is_none() {
		let mut folders: Vec<String> = Vec::new();
		for d in all.iter().flat_map(|t| &t.dirs).filter_map(|d| d.parent()) {
			let d = tpfs_core::sanitize(d.as_os_str());
			if !folders.contains(&d) {
				folders.push(d);
			}
		}
		if all.is_empty() {
			println!("The trash is empty.");
			return Ok(ExitCode::SUCCESS);
		}
		eprintln!("Trash: {}", folders.join(", "));
		println!("{header}");
		for t in &all {
			println!("{}", line(t));
		}
		println!("Total: {}", totals(&all));
		return Ok(ExitCode::SUCCESS);
	}

	// `tpfs trash empty`: by default everything older than 30 days; `--batch` alone ignores age.
	let days = args.older_than.or(if args.batch.is_some() { None } else { Some(30) });
	if let Some(id) = &args.batch {
		let shown = tpfs_core::sanitize(id.as_ref());
		match all.iter().find(|t| t.id == *id) {
			None => return Err(usage(format!("batch {shown} has nothing in the trash (see `tpfs trash`)"))),
			Some(t) if !t.emptiable() => {
				return Err(format!(
					"batch {shown} was interrupted and `tpfs recover` needs its trash; run that first"
				)
				.into());
			}
			_ => {}
		}
	}
	let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
	let wanted = |t: &Trashed| {
		args.batch.as_ref().is_none_or(|b| *b == t.id) && days.is_none_or(|d| t.at.saturating_add(d * 86_400) <= now)
	};
	let kept = all.iter().filter(|t| wanted(t) && !t.emptiable()).count();
	// An emptying cut short is finished whatever is chosen: it is recorded already.
	let chosen: Vec<Trashed> = all.into_iter().filter(|t| t.purged || (wanted(t) && t.emptiable())).collect();
	if kept > 0 {
		eprintln!("{kept} interrupted batch(es) keep their trash: `tpfs recover` needs it.");
	}
	if chosen.is_empty() {
		match days {
			Some(d) => println!("Nothing to empty: nothing in the trash is older than {d} day(s) (see `tpfs trash`)."),
			None => println!("Nothing to empty."),
		}
		return Ok(ExitCode::SUCCESS);
	}
	let token = trash::token(&chosen);
	println!("This will be deleted for good:");
	println!("{header}");
	for t in &chosen {
		println!("{}", line(t));
		for (inside, from) in t.entries.iter().take(20) {
			match from {
				Some(from) => println!("    {}", tpfs_core::sanitize(from.as_os_str())),
				None => println!("    {}", tpfs_core::sanitize(inside.as_os_str())),
			}
		}
		if t.entries.len() > 20 {
			println!("    … and {} more", t.entries.len() - 20);
		}
	}
	println!("Total: {}", totals(&chosen));
	let undoable = chosen.iter().filter(|t| t.undoable()).count();
	if undoable > 0 {
		println!(
			"{undoable} of these batch(es) can still be undone; afterwards they (and the batches before them) cannot."
		);
	}
	println!("Token: {token}");
	if let Some(approved) = &args.approve {
		if *approved != token {
			return Err(cancelled("--approve does not match this selection's token; nothing was deleted"));
		}
	} else if io::stdin().is_terminal() && io::stderr().is_terminal() {
		drop(journal); // not held while the user reads
		if ask("Type 'yes' to delete this for good: ")? != "yes" {
			return Err(cancelled("cancelled; nothing was deleted"));
		}
		journal = open_state(args)?.0;
	} else {
		return Err(cancelled(format!("not interactive: pass --approve {token} to delete exactly this selection")));
	}
	let ids: Vec<String> = chosen.iter().map(|t| t.id.clone()).collect();
	let report = match trash::empty(&mut journal, &trash_dir, &ids, &token) {
		Ok(r) => r,
		Err(ExecError::Blocked(m)) => return Err(cancelled(m)),
		Err(e) => return Err(e.to_string().into()),
	};
	println!("Deleted {} item(s) of {} batch(es), {} freed.", report.items, report.batches, size(report.bytes));
	for e in &report.errors {
		eprintln!("  error: {e}");
	}
	if report.errors.is_empty() {
		Ok(ExitCode::SUCCESS)
	} else {
		eprintln!("Fix the cause, then run `tpfs trash empty` again: it finishes what was left.");
		Ok(ExitCode::from(1))
	}
}

/// `1.5 MiB`, `120 B`.
fn size(bytes: u64) -> String {
	const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
	let (mut v, mut u) = (bytes as f64, 0);
	while v >= 1024.0 && u + 1 < UNITS.len() {
		v /= 1024.0;
		u += 1;
	}
	if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// `YYYY-MM-DD HH:MM` in UTC (`????-??-?? ??:??` when unknown).
fn date(secs: u64) -> String {
	if secs == 0 {
		return "????-??-?? ??:??".into();
	}
	// Days to a civil date (Howard Hinnant's algorithm).
	let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
	let z = days + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z - era * 146_097;
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let m = if mp < 10 { mp + 3 } else { mp - 9 };
	let y = yoe + era * 400 + i64::from(m <= 2);
	format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rest / 3600, rest % 3600 / 60)
}

fn check(args: &Args, apply: bool) -> Res {
	let plan = read_plan(args.plan.as_deref().ok_or_else(|| usage("missing plan file (use - for stdin)"))?)
		.map_err(|e| if e.starts_with("not a valid plan") { Fail(3, e) } else { e.into() })?;
	if args.roots.is_empty() {
		return Err(usage("at least one --root <dir> is required"));
	}
	if plan.ops.is_empty() {
		// Like `sync` and `undo` with nothing to do: not a rejection.
		println!("Nothing to do: the plan has no operations.");
		return Ok(ExitCode::SUCCESS);
	}
	let mut validator =
		Validator::new(&args.roots).map_err(|e| root_error(&args.roots, e))?.with_protected([state_dir(args)?]);
	if let Some(n) = args.max_ops {
		// A big plan you made on purpose (a photo clean-up); agents keep the default limit.
		if args.source == Source::Agent {
			return Err(usage("--max-ops is for human plans only"));
		}
		validator = validator.with_max_ops(n);
	}

	let Some((digest, level)) = present(&validator, &plan, args.source) else { return Ok(rejected(args)) };
	if !apply {
		println!("OK — the plan is valid.");
		return Ok(ExitCode::SUCCESS);
	}
	// The state folder is checked before the question, not after the user said yes; the journal is
	// not held while the user reads, so the commander and the MCP server keep working meanwhile.
	drop(open_state(args)?);
	confirm(args, &digest, level)?;
	execute(args, &validator, &plan, open_state(args)?)
}

/// Print the plan, its risk and digest, and any problems. `None` when the plan has problems.
fn present(validator: &Validator, plan: &Plan, source: Source) -> Option<(String, Level)> {
	let issues = validator.validate(plan);
	let risk = risk::assess(plan, source);
	let digest = plan.digest();

	if !plan.reason.is_empty() {
		println!("Plan: {}", tpfs_core::sanitize(plan.reason.as_ref()));
	}
	print_preview(plan);
	println!(
		"Risk: {}{}",
		level_word(risk.level),
		if risk.reasons.is_empty() { String::new() } else { format!(" — {}", risk.reasons.join("; ")) }
	);
	println!("Digest: {digest}");

	if !issues.is_empty() {
		println!("Problems:");
		for i in &issues {
			println!("  {i}");
		}
		return None;
	}
	Some((digest, risk.level))
}

/// The operation list, totals and a trash line — the same for `check`, `apply`, `sync` and `approve`.
/// Steps shown at once when a long plan is reviewed in a terminal.
const PAGE: usize = 30;

fn print_preview(plan: &Plan) {
	// On a terminal, what goes to the trash or removes a folder stands out (as in the commander).
	let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
	// A long plan is shown a page at a time when someone is reading it, so no step scrolls past
	// unseen before the question.
	let paged = io::stdout().is_terminal() && io::stdin().is_terminal();
	for (n, line) in plan.preview().into_iter().enumerate() {
		if paged && n > 0 && n % PAGE == 0 {
			print!("  -- {n} of {} steps shown; Enter for more --", plan.ops.len());
			io::stdout().flush().ok();
			let mut skip = String::new();
			let _ = io::stdin().lock().read_line(&mut skip);
		}
		if color && (line.starts_with("TRASH") || line.starts_with("RMDIR")) {
			println!("  \x1b[1;31m{line}\x1b[0m");
		} else {
			println!("  {line}");
		}
	}
	println!("Total: {} operation(s): {}", plan.ops.len(), risk::totals_line(plan, ", "));
	let trashed = plan.ops.iter().filter(|op| matches!(op, tpfs_core::Op::Trash { .. })).count();
	if trashed > 0 {
		println!("{trashed} item(s) go to the trash (nothing is deleted; `tpfs undo` puts them back).");
	}
}

/// Exit 3, saying so on stderr (the problems themselves were listed on stdout).
fn rejected(args: &Args) -> ExitCode {
	eprintln!("tpfs {}: plan rejected (see the problems above); nothing was changed", args.command);
	ExitCode::from(3)
}

/// The same words as the commander's confirmation.
fn level_word(level: Level) -> &'static str {
	match level {
		Level::Low => "low",
		Level::Medium => "MEDIUM",
		_ => "HIGH",
	}
}

fn root_error(roots: &[PathBuf], e: io::Error) -> Fail {
	let shown: Vec<String> = roots.iter().map(|r| tpfs_core::sanitize(r.as_os_str())).collect();
	usage(format!("--root {}: {e}", shown.join(", ")))
}

fn execute(args: &Args, validator: &Validator, plan: &Plan, (mut journal, trash): (Journal, PathBuf)) -> Res {
	match Executor::new(validator, &mut journal, trash).execute(plan, args.source) {
		Ok(b) => {
			println!("Done: {} step(s), batch {}. Undo with `tpfs undo`.", b.steps, b.id);
			Ok(ExitCode::SUCCESS)
		}
		Err(ExecError::Rejected(issues)) => {
			for i in &issues {
				eprintln!("  {i}");
			}
			Ok(rejected(args))
		}
		Err(e) => Err(e.to_string().into()),
	}
}

/// `tpfs sync <source> <target>`: compare, build a plan of copy/trash, show it; apply only with
/// `--apply` after confirmation (or `--approve <digest>` from a previous dry run).
fn sync(args: &Args) -> Res {
	if args.json && (args.do_apply || args.approve.is_some()) {
		return Err(usage("--json only prints the plan; drop it to apply, or pass the plan to `tpfs apply`"));
	}
	use tpfs_core::sync::{Direction, SyncOptions, compare, sync_plan};

	let dir = |v: &Option<OsString>, what: &str| -> Result<PathBuf, Fail> {
		let v = v.as_deref().ok_or_else(|| usage(format!("missing {what} folder: tpfs sync <source> <target>")))?;
		let shown = tpfs_core::sanitize(v);
		let p = tpfs_core::canonicalize(Path::new(v)).map_err(|e| usage(format!("{shown}: {e}")))?;
		if p.is_dir() { Ok(p) } else { Err(usage(format!("not a folder: {shown}"))) }
	};
	let (source, target) = (dir(&args.plan, "source")?, dir(&args.extra, "target")?);
	if source.starts_with(&target) || target.starts_with(&source) {
		return Err(usage("source and target must be separate folders (one is inside the other)"));
	}
	if args.both && args.mirror {
		return Err(usage("--mirror only works one way; drop --both or --mirror"));
	}

	let cmp = compare(&source, &target, true).map_err(|e| e.to_string())?;
	if cmp.truncated {
		return Err(format!("too many files to compare (limit {})", tpfs_core::sync::MAX_ENTRIES).into());
	}
	let direction = if args.both { Direction::Both } else { Direction::LeftToRight };
	let sp = sync_plan(&source, &target, &cmp, SyncOptions { direction, mirror: args.mirror });

	if args.json {
		println!("{}", serde_json::to_string_pretty(&sp.plan).map_err(|e| e.to_string())?);
		eprintln!("{} operation(s), {} skipped, {} identical", sp.plan.ops.len(), sp.skipped.len(), cmp.same);
		return Ok(ExitCode::SUCCESS);
	}

	println!("{} difference(s), {} identical file(s).", cmp.items.len(), cmp.same);
	for s in &sp.skipped {
		if s.rel.as_os_str().is_empty() {
			println!("  {}", s.why);
		} else {
			println!("  skip  {}  ({})", tpfs_core::sanitize(s.rel.as_os_str()), s.why);
		}
	}
	if sp.plan.ops.is_empty() {
		println!("Nothing to sync.");
		return Ok(ExitCode::SUCCESS);
	}

	let validator = Validator::new([&source, &target]).map_err(|e| e.to_string())?.with_protected([state_dir(args)?]);
	let Some((digest, level)) = present(&validator, &sp.plan, args.source) else { return Ok(rejected(args)) };
	if !args.do_apply && args.approve.is_none() {
		println!("Dry run. Apply with --apply, or non-interactively with --approve {digest}");
		return Ok(ExitCode::SUCCESS);
	}
	drop(open_state(args)?);
	confirm(args, &digest, level)?;
	execute(args, &validator, &sp.plan, open_state(args)?)
}

/// An approval is bound to the exact plan: either the digest is passed explicitly,
/// or a human types `yes` on a terminal. Agents never get a `--yes`.
fn confirm(args: &Args, digest: &str, level: Level) -> Result<(), Fail> {
	if let Some(approved) = &args.approve {
		return if approved == digest {
			Ok(())
		} else {
			Err(cancelled("--approve does not match this plan's digest; nothing was changed"))
		};
	}
	if args.source == Source::Agent {
		return Err(cancelled("plans from agents need a human approval: --approve <digest>"));
	}
	if !io::stdin().is_terminal() || !io::stderr().is_terminal() || args.plan.as_deref() == Some("-".as_ref()) {
		return Err(cancelled("not interactive: pass --approve <digest> to apply exactly this plan"));
	}

	let prompt = if level == Level::High { "Type 'yes' to apply this HIGH-risk plan: " } else { "Apply? [y/N] " };
	let answer = ask(prompt)?;
	let ok = if level == Level::High { answer == "yes" } else { matches!(answer.as_str(), "y" | "Y" | "yes") };
	if ok { Ok(()) } else { Err(cancelled("cancelled; nothing was changed")) }
}

/// Ask on the terminal: the prompt goes to stderr, so it is seen even when stdout is redirected.
fn ask(prompt: &str) -> Result<String, Fail> {
	eprint!("{prompt}");
	io::stderr().flush().ok();
	let mut answer = String::new();
	let n = io::stdin().lock().read_line(&mut answer).map_err(|e| e.to_string())?;
	if n == 0 {
		return Err(cancelled("no answer (end of input); nothing was changed"));
	}
	Ok(answer.trim().to_owned())
}

/// A human reviews an agent's plan in their own terminal and approves exactly that plan.
/// Deliberately interactive only: there is no flag an agent could pass to approve its own plan.
fn approve(args: &Args) -> Res {
	let id = plan_id(args)?;
	let store = tpfs_mcp::Store::new(&state_dir(args)?);
	let pending = find_pending(&store, &id)?;
	if store.is_claimed(&id) {
		return Err(usage(format!(
			"plan {} was already started once and is used up (see `tpfs log`); reject it",
			tpfs_core::sanitize(id.as_ref())
		)));
	}
	if !show_pending(args, &pending)? {
		return Ok(rejected(args));
	}
	if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
		return Err(cancelled("approval must be given interactively in a terminal"));
	}
	if ask("Type 'yes' to let the agent apply exactly this plan: ")? != "yes" {
		return Err(cancelled("not approved"));
	}
	store.approve(&pending).map_err(|e| e.to_string())?;
	println!("Approved. The agent can now call apply_plan for {id}.");
	Ok(ExitCode::SUCCESS)
}

/// Show an agent's pending plan as the person must see it before approving: the steps, the
/// allowed folders, the risk, the digest and any problems. `false` when it cannot be approved.
fn show_pending(args: &Args, pending: &tpfs_mcp::Pending) -> Result<bool, Fail> {
	if pending.plan.digest() != pending.digest {
		return Err("the stored plan was modified after it was proposed; reject it".into());
	}
	let validator =
		Validator::new(&pending.roots).map_err(|e| root_error(&pending.roots, e))?.with_protected([state_dir(args)?]);
	let issues = validator.validate(&pending.plan);
	let risk = risk::assess(&pending.plan, Source::Agent);

	println!("Plan {} proposed by an agent:", tpfs_core::sanitize(pending.id.as_ref()));
	if !pending.plan.reason.is_empty() {
		println!("  \"{}\"", tpfs_core::sanitize(pending.plan.reason.as_ref()));
	}
	print_preview(&pending.plan);
	println!(
		"Allowed folders: {}",
		pending.roots.iter().map(|r| tpfs_core::sanitize(r.as_os_str())).collect::<Vec<_>>().join(", ")
	);
	println!(
		"Risk: {}{}",
		level_word(risk.level),
		if risk.reasons.is_empty() { String::new() } else { format!(" — {}", risk.reasons.join("; ")) }
	);
	println!("Digest: {}", pending.digest);
	if !issues.is_empty() {
		println!("Problems:");
		for i in &issues {
			println!("  {i}");
		}
		return Ok(false);
	}
	Ok(true)
}

/// `tpfs watch`: wait for plans proposed by agents and review each one as it arrives, in this
/// terminal: approve it (`yes`), reject it (`no`), or leave it for later (Enter). The same
/// review and the same `yes` as `tpfs approve`, without copying plan ids between terminals.
fn watch(args: &Args) -> Res {
	if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
		return Err(cancelled("tpfs watch reviews plans interactively: run it in a terminal"));
	}
	let state = state_dir(args)?;
	tpfs_core::private_dir(&state).map_err(|e| e.to_string())?;
	let store = tpfs_mcp::Store::new(&state);
	eprintln!(
		"Watching for plans proposed by agents (state: {}). Ctrl+C to stop.",
		tpfs_core::sanitize(state.as_os_str())
	);
	let mut seen = std::collections::HashSet::new();
	let recipes = tpfs_mcp::RecipeStore::new(&state);
	loop {
		let (proposals, _) = recipes.scan().map_err(|e| e.to_string())?;
		for p in proposals {
			if !seen.insert(format!("recipe:{}", p.id)) {
				continue;
			}
			println!();
			let id = tpfs_core::sanitize(p.id.as_ref());
			recipe_cli::describe_proposal(&p);
			let roots = match recipe_cli::resolve(args, &p) {
				Ok(r) => r,
				Err(Fail(_, e)) => {
					eprintln!("tpfs watch: it cannot be installed as proposed: {e}");
					eprintln!(
						"tpfs watch: `tpfs recipe install {id} --root <dir>` installs it for chosen folders; `tpfs recipe reject {id}` drops it."
					);
					continue;
				}
			};
			let answer = ask("Type 'yes' to install this recipe, 'no' to reject it, or Enter to decide later: ")?;
			match answer.as_str() {
				"yes" => {
					if let Err(Fail(_, e)) = recipe_cli::finish(args, &p, &roots) {
						eprintln!("tpfs watch: not installed: {e}");
					}
				}
				"no" | "reject" => {
					recipes.reject(&p.id).map_err(|e| e.to_string())?;
					println!("Rejected recipe {id}.");
				}
				_ => println!("Left waiting: `tpfs recipe install {id}` or `tpfs recipe reject {id}` later."),
			}
		}
		let (plans, _) = store.scan().map_err(|e| e.to_string())?;
		for pending in plans {
			if !seen.insert(pending.id.clone()) || store.is_approved(&pending) || store.is_claimed(&pending.id) {
				continue;
			}
			let id = tpfs_core::sanitize(pending.id.as_ref());
			println!();
			match show_pending(args, &pending) {
				Ok(true) => {}
				Ok(false) => {
					println!("It cannot be approved as it is. `tpfs reject {id}` drops it.");
					continue;
				}
				Err(Fail(_, e)) => {
					println!("{e}");
					continue;
				}
			}
			let answer = ask("Type 'yes' to approve exactly this plan, 'no' to reject it, or Enter to decide later: ")?;
			match answer.as_str() {
				"yes" => {
					store.approve(&pending).map_err(|e| e.to_string())?;
					println!("Approved. The agent can now call apply_plan for {id}.");
				}
				"no" | "reject" => {
					store.reject(&pending.id).map_err(|e| e.to_string())?;
					println!("Rejected {id}.");
				}
				_ => println!("Left pending: `tpfs approve {id}` or `tpfs reject {id}` later."),
			}
		}
		std::thread::sleep(std::time::Duration::from_millis(700));
	}
}

/// A pending plan by id; an unknown id is a usage error that points to the list.
fn find_pending(store: &tpfs_mcp::Store, id: &str) -> Result<tpfs_mcp::Pending, Fail> {
	store.get(id).map_err(|e| match e.kind() {
		io::ErrorKind::NotFound | io::ErrorKind::InvalidInput => usage(format!("{e} (see `tpfs pending`)")),
		_ => e.to_string().into(),
	})
}

fn plan_id(args: &Args) -> Result<String, Fail> {
	args.plan.as_ref().and_then(|p| p.to_str()).map(str::to_owned).ok_or_else(|| usage("missing <planId>"))
}

fn read_plan(source: &std::ffi::OsStr) -> Result<Plan, String> {
	// A plan holds at most MAX_OPS steps of bounded size; anything far larger is not a plan.
	const MAX_PLAN_BYTES: u64 = 64 << 20;
	let name = tpfs_core::sanitize(source);
	let input: Box<dyn Read> = if source == "-" {
		Box::new(io::stdin())
	} else {
		Box::new(fs::File::open(source).map_err(|e| format!("{name}: {e}"))?)
	};
	let mut text = String::new();
	input.take(MAX_PLAN_BYTES + 1).read_to_string(&mut text).map_err(|e| format!("{name}: {e}"))?;
	if text.len() as u64 > MAX_PLAN_BYTES {
		return Err(format!("{name}: larger than {} MiB; not a plan", MAX_PLAN_BYTES >> 20));
	}
	serde_json::from_str(&text)
		.map_err(|e| format!("not a valid plan: {}", tpfs_core::sanitize(e.to_string().as_ref())))
}

fn state_dir(args: &Args) -> Result<PathBuf, String> {
	if let Some(h) = &args.home {
		return Ok(h.clone());
	}
	if let Some(h) = std::env::var_os("TPFS_HOME") {
		return Ok(h.into());
	}
	if cfg!(windows)
		&& let Some(x) = std::env::var_os("LOCALAPPDATA").filter(|x| !x.is_empty())
	{
		return Ok(PathBuf::from(x).join("tpfs"));
	}
	if let Some(x) = std::env::var_os("XDG_STATE_HOME").filter(|x| !x.is_empty()) {
		return Ok(PathBuf::from(x).join("tpfs"));
	}
	let home = std::env::var_os("HOME")
		.or_else(|| std::env::var_os("USERPROFILE"))
		.ok_or("cannot determine the home directory; use --home")?;
	Ok(PathBuf::from(home).join(".local/state/tpfs"))
}

fn open_state(args: &Args) -> Result<(Journal, PathBuf), String> {
	let dir = state_dir(args)?;
	let journal = Journal::open(dir.join("journal.jsonl")).map_err(|e| e.to_string())?;
	Ok((journal, dir.join("trash")))
}

/// Undo and recovery replay what the journal recorded anywhere the user works: every drive or
/// `/`, plus (Windows) the network shares the journal names. Below them symlinks are still refused,
/// and tpfs's own state stays protected.
fn any_validator(args: &Args, journal: &mut Journal) -> Result<Validator, String> {
	let mut roots = tpfs_core::system_roots();
	for b in journal.batches().map_err(|e| e.to_string())? {
		for step in &b.steps {
			for p in step.forward.paths().into_iter().chain(step.inverse.paths()) {
				if let Some(share) = tpfs_core::share_root(p)
					&& !roots.contains(&share)
					&& share.is_dir()
				{
					roots.push(share);
				}
			}
		}
	}
	Ok(Validator::new(roots).map_err(|e| e.to_string())?.with_protected([state_dir(args)?]))
}
