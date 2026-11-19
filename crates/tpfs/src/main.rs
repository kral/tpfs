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
  tpfs apply  <plan.json|-> --root <dir>...     validate, confirm, execute
                [--approve <digest>]              non-interactive approval of exactly this plan
                [--source human|agent]            who proposed the plan (default: human)
                [--max-ops <n>]                   allow a longer human plan (default 10000)
  tpfs sync <source> <target>                   compare two folders and show the sync plan
                [--both]                          copy both ways, the newer file wins
                [--mirror]                        one-way: move target-only items to the trash
                [--apply]                         confirm and execute (or --approve <digest>)
                [--json]                          print the plan as JSON (for `tpfs apply -`)
  tpfs undo                                     revert the most recent batch
  tpfs log                                      list recorded batches
  tpfs recover                                  roll back batches interrupted by a crash

  tpfs mcp --root <dir>... [--approval terminal|client]
                                                  MCP server on stdio for AI agents (agents propose,
                                                  humans approve; default approval: terminal)
  tpfs pending                                  list plans proposed by agents
  tpfs approve <planId>                         review and approve an agent's plan (interactive)
  tpfs reject <planId>                          discard an agent's plan

OPTIONS:
  --home <dir>    state directory (journal, trash). Default: $TPFS_HOME, else
                  $XDG_STATE_HOME/tpfs or ~/.local/state/tpfs (Unix), %LOCALAPPDATA%\\tpfs (Windows)
  -h, --help      show this help (`tpfs <command> --help` for one command)
  -V, --version   print the version

EXIT CODES:
  0  done (also when there was nothing to do)    3  plan rejected by validation
  1  failed                                      4  cancelled or not approved; nothing changed
  2  usage error

Nothing is ever deleted permanently: `trash` moves into <home>/trash and can be undone.
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
			inside = line.starts_with(&start) && line[start.len()..].chars().next().is_none_or(|c| c == ' ');
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
		"undo" | "log" | "recover" | "pending" => (&["home"], 0),
		"mcp" => (&["root", "approval", "home"], 0),
		"approve" | "reject" => (&["home"], 1),
		_ => return None,
	})
}

struct Args {
	command: String,
	plan: Option<OsString>,
	extra: Option<OsString>,
	roots: Vec<PathBuf>,
	approve: Option<String>,
	approval: tpfs_mcp::Approval,
	source: Source,
	home: Option<PathBuf>,
	icons: bool,
	both: bool,
	mirror: bool,
	do_apply: bool,
	json: bool,
	max_ops: Option<usize>,
	/// Flags given, to reject ones the command does not use.
	given: Vec<&'static str>,
	positional: usize,
	help: bool,
}

fn main() -> ExitCode {
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
		roots: vec![],
		approve: None,
		approval: tpfs_mcp::Approval::Terminal,
		source: Source::Human,
		home: None,
		icons: false,
		both: false,
		mirror: false,
		do_apply: false,
		json: false,
		max_ops: None,
		given: vec![],
		positional: 0,
		help: false,
	};

	let e = |e: lexopt::Error| usage(e.to_string());
	while let Some(arg) = parser.next().map_err(e)? {
		if let Long(name) = &arg {
			const FLAGS: [&str; 11] = [
				"root", "approve", "approval", "icons", "both", "mirror", "apply", "json", "max-ops", "home", "source",
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
			Long("root") => args.roots.push(parser.value().map_err(|e| e.to_string())?.into()),
			Long("approve") => {
				args.approve = Some(parser.value().map_err(|e| e.to_string())?.string().map_err(|e| e.to_string())?)
			}
			Long("approval") => {
				args.approval =
					match parser.value().map_err(|e| e.to_string())?.string().map_err(|e| e.to_string())?.as_str() {
						"terminal" => tpfs_mcp::Approval::Terminal,
						"client" => tpfs_mcp::Approval::Client,
						other => return Err(usage(format!("unknown approval mode '{other}' (terminal|client)"))),
					}
			}
			Long("icons") => args.icons = true,
			Long("both") => args.both = true,
			Long("mirror") => args.mirror = true,
			Long("apply") => args.do_apply = true,
			Long("json") => args.json = true,
			Long("max-ops") => {
				let v = parser.value().map_err(|e| e.to_string())?.string().map_err(|e| e.to_string())?;
				args.max_ops = Some(v.parse().map_err(|_| usage(format!("--max-ops needs a number, got '{v}'")))?);
			}
			Long("home") => args.home = Some(parser.value().map_err(|e| e.to_string())?.into()),
			Long("source") => {
				args.source =
					match parser.value().map_err(|e| e.to_string())?.string().map_err(|e| e.to_string())?.as_str() {
						"human" => Source::Human,
						"agent" => Source::Agent,
						other => return Err(usage(format!("unknown source '{other}' (human|agent)"))),
					}
			}
			Value(v) if args.command.is_empty() => args.command = v.string().map_err(|e| e.to_string())?,
			Value(v) if args.plan.is_none() => args.plan = Some(v),
			Value(v) if args.extra.is_none() => args.extra = Some(v),
			other => return Err(usage(other.unexpected().to_string())),
		}
	}

	// `tpfs <dir> [<dir>]` opens the commander there.
	const COMMANDS: [&str; 12] =
		["ui", "schema", "check", "apply", "sync", "undo", "log", "recover", "mcp", "pending", "approve", "reject"];
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
		"check" => check(&args, false),
		"apply" => check(&args, true),
		"sync" => sync(&args),
		"undo" => {
			let (mut journal, trash) = open_state(&args)?;
			let validator = any_validator(&args)?;
			let last = journal
				.batches()
				.map_err(|e| e.to_string())?
				.into_iter()
				.rev()
				.find(|b| b.status == BatchStatus::Committed);
			if let Some(b) = &last
				&& io::stdin().is_terminal()
				&& io::stderr().is_terminal()
			{
				eprintln!(
					"Undo batch {} by {} — {} step(s): {}",
					tpfs_core::sanitize(b.id.as_ref()),
					tpfs_core::sanitize(b.source.as_ref()),
					b.steps.len(),
					tpfs_core::sanitize(b.reason.as_ref())
				);
				for s in b.steps.iter().filter(|s| s.done && !s.undone).take(20) {
					eprintln!("  {}", s.forward);
				}
				if !ask("Undo it? [y/N] ")?.eq_ignore_ascii_case("y") {
					return Err(cancelled("cancelled; nothing was changed"));
				}
			}
			match Executor::new(&validator, &mut journal, trash).undo_last() {
				Ok(Some(b)) => println!("Undone batch {} ({} step(s)).", b.id, b.steps),
				Ok(None) => println!("Nothing to undo."),
				Err(e) => return Err(e.to_string().into()),
			}
			Ok(ExitCode::SUCCESS)
		}
		"log" => {
			let (mut journal, _) = open_state(&args)?;
			eprintln!("State: {}", tpfs_core::sanitize(state_dir(&args)?.as_os_str()));
			let batches = journal.batches().map_err(|e| e.to_string())?;
			if batches.is_empty() {
				println!("No batches recorded.");
			}
			for b in &batches {
				let status = match b.status {
					BatchStatus::Committed => "committed",
					BatchStatus::Aborted => "rolled back",
					BatchStatus::Undone => "undone",
					BatchStatus::Incomplete => "INCOMPLETE (run `tpfs recover`)",
					_ => "unknown",
				};
				let status = if b.damaged { "DAMAGED (see the journal)" } else { status };
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
			Ok(ExitCode::SUCCESS)
		}
		"recover" => {
			let (mut journal, trash) = open_state(&args)?;
			let validator = any_validator(&args)?;
			let report = Executor::new(&validator, &mut journal, trash).recover().map_err(|e| e.to_string())?;
			println!("Rolled back {} interrupted batch(es).", report.rolled_back.len());
			for e in &report.errors {
				eprintln!("  error: {e}");
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
			let config =
				tpfs_mcp::Config { roots: args.roots.clone(), state: state_dir(&args)?, approval: args.approval };
			let mut server = tpfs_mcp::Server::new(config).map_err(|e| format!("invalid root: {e}"))?;
			eprintln!("tpfs mcp: serving on stdio");
			server.run(io::stdin().lock(), io::stdout().lock()).map_err(|e| e.to_string())?;
			Ok(ExitCode::SUCCESS)
		}
		"pending" => {
			let store = tpfs_mcp::Store::new(&state_dir(&args)?);
			let pending = store.list().map_err(|e| e.to_string())?;
			if pending.is_empty() {
				println!("No pending plans.");
			}
			for p in &pending {
				let approved = if store.is_approved(p) { "approved" } else { "waiting" };
				println!(
					"{}  {:<8} {} op(s)  {}",
					p.id,
					approved,
					p.plan.ops.len(),
					tpfs_core::sanitize(p.plan.reason.as_ref())
				);
			}
			Ok(ExitCode::SUCCESS)
		}
		"approve" => approve(&args),
		"reject" => {
			let id = plan_id(&args)?;
			let store = tpfs_mcp::Store::new(&state_dir(&args)?);
			store.get(&id).map_err(|e| e.to_string())?;
			store.remove(&id).map_err(|e| e.to_string())?;
			println!("Rejected {id}.");
			Ok(ExitCode::SUCCESS)
		}
		other => Err(usage(format!("unknown command '{other}' (see `tpfs --help`)"))),
	}
}

fn check(args: &Args, apply: bool) -> Res {
	let plan = read_plan(args.plan.as_deref().ok_or_else(|| usage("missing plan file (use - for stdin)"))?)?;
	if args.roots.is_empty() {
		return Err(usage("at least one --root <dir> is required"));
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

	let Some((digest, level)) = present(&validator, &plan, args.source) else { return Ok(ExitCode::from(3)) };
	if !apply {
		println!("OK — the plan is valid.");
		return Ok(ExitCode::SUCCESS);
	}
	confirm(args, &digest, level)?;
	execute(args, &validator, &plan)
}

/// Print the plan, its risk and digest, and any problems. `None` when the plan has problems.
fn present(validator: &Validator, plan: &Plan, source: Source) -> Option<(String, Level)> {
	let issues = validator.validate(plan);
	let risk = risk::assess(plan, source);
	let digest = plan.digest();

	if !plan.reason.is_empty() {
		println!("Plan: {}", tpfs_core::sanitize(plan.reason.as_ref()));
	}
	for line in plan.preview() {
		println!("  {line}");
	}
	println!("Total: {} operation(s): {}", plan.ops.len(), totals(plan));
	println!(
		"Risk: {:?}{}",
		risk.level,
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

/// Counts per kind of operation, e.g. "3 move, 1 trash".
fn totals(plan: &Plan) -> String {
	let mut counts: Vec<(&str, usize)> = Vec::new();
	for op in &plan.ops {
		let k = op.to_string().split_whitespace().next().unwrap_or("").to_lowercase();
		let k: &str = match k.as_str() {
			"move" => "move",
			"copy" => "copy",
			"rename" => "rename",
			"trash" => "trash",
			"mkdir" => "mkdir",
			_ => "rmdir",
		};
		match counts.iter_mut().find(|(n, _)| *n == k) {
			Some((_, c)) => *c += 1,
			None => counts.push((k, 1)),
		}
	}
	counts.iter().map(|(k, c)| format!("{c} {k}")).collect::<Vec<_>>().join(", ")
}

fn root_error(roots: &[PathBuf], e: io::Error) -> Fail {
	let shown: Vec<String> = roots.iter().map(|r| tpfs_core::sanitize(r.as_os_str())).collect();
	usage(format!("--root {}: {e}", shown.join(", ")))
}

fn execute(args: &Args, validator: &Validator, plan: &Plan) -> Res {
	let (mut journal, trash) = open_state(args)?;
	match Executor::new(validator, &mut journal, trash).execute(plan, args.source) {
		Ok(b) => {
			println!("Done: {} step(s), batch {}. Undo with `tpfs undo`.", b.steps, b.id);
			Ok(ExitCode::SUCCESS)
		}
		Err(ExecError::Rejected(issues)) => {
			for i in &issues {
				eprintln!("  {i}");
			}
			Ok(ExitCode::from(3))
		}
		Err(e) => Err(e.to_string().into()),
	}
}

/// `tpfs sync <source> <target>`: compare, build a plan of copy/trash, show it; apply only with
/// `--apply` after confirmation (or `--approve <digest>` from a previous dry run).
fn sync(args: &Args) -> Res {
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
		println!("  skip  {}  ({})", tpfs_core::sanitize(s.rel.as_os_str()), s.why);
	}
	if sp.plan.ops.is_empty() {
		println!("Nothing to sync.");
		return Ok(ExitCode::SUCCESS);
	}

	let validator = Validator::new([&source, &target]).map_err(|e| e.to_string())?.with_protected([state_dir(args)?]);
	let Some((digest, level)) = present(&validator, &sp.plan, args.source) else { return Ok(ExitCode::from(3)) };
	if !args.do_apply && args.approve.is_none() {
		println!("Dry run. Apply with --apply, or non-interactively with --approve {digest}");
		return Ok(ExitCode::SUCCESS);
	}
	confirm(args, &digest, level)?;
	execute(args, &validator, &sp.plan)
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
	let pending = store.get(&id).map_err(|e| e.to_string())?;
	if pending.plan.digest() != pending.digest {
		return Err("the stored plan was modified after it was proposed; reject it".into());
	}

	let validator =
		Validator::new(&pending.roots).map_err(|e| root_error(&pending.roots, e))?.with_protected([state_dir(args)?]);
	let issues = validator.validate(&pending.plan);
	let risk = risk::assess(&pending.plan, Source::Agent);

	println!("Plan {id} proposed by an agent:");
	if !pending.plan.reason.is_empty() {
		println!("  \"{}\"", tpfs_core::sanitize(pending.plan.reason.as_ref()));
	}
	for line in pending.plan.preview() {
		println!("  {line}");
	}
	println!(
		"Allowed folders: {}",
		pending.roots.iter().map(|r| tpfs_core::sanitize(r.as_os_str())).collect::<Vec<_>>().join(", ")
	);
	println!("Risk: {:?} — {}", risk.level, risk.reasons.join("; "));
	if !issues.is_empty() {
		println!("Problems:");
		for i in &issues {
			println!("  {i}");
		}
		return Ok(ExitCode::from(3));
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

fn plan_id(args: &Args) -> Result<String, Fail> {
	args.plan.as_ref().and_then(|p| p.to_str()).map(str::to_owned).ok_or_else(|| usage("missing <planId>"))
}

fn read_plan(source: &std::ffi::OsStr) -> Result<Plan, String> {
	let text = if source == "-" {
		let mut s = String::new();
		io::stdin().read_to_string(&mut s).map_err(|e| e.to_string())?;
		s
	} else {
		fs::read_to_string(source).map_err(|e| format!("{}: {e}", Path::new(source).display()))?
	};
	serde_json::from_str(&text).map_err(|e| format!("not a valid plan: {e}"))
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

/// Undo and recovery replay what the journal recorded anywhere the user works; below every
/// system root they still refuse symlinks, and tpfs's own state stays protected.
fn any_validator(args: &Args) -> Result<Validator, String> {
	Ok(Validator::new(tpfs_core::system_roots()).map_err(|e| e.to_string())?.with_protected([state_dir(args)?]))
}
