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
  --home <dir>    state directory (journal, trash). Default: $TPFS_HOME,
                  $XDG_STATE_HOME/tpfs or ~/.local/state/tpfs
  -h, --help      show this help
  -V, --version   print the version

Nothing is ever deleted permanently: `trash` moves into <home>/trash and can be undone.
";

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
}

fn main() -> ExitCode {
	match parse_args().and_then(run) {
		Ok(code) => code,
		Err(e) => {
			eprintln!("tpfs: {e}");
			ExitCode::from(1)
		}
	}
}

fn parse_args() -> Result<Args, String> {
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
	};

	while let Some(arg) = parser.next().map_err(|e| e.to_string())? {
		match arg {
			Short('h') | Long("help") => {
				print!("{USAGE}");
				std::process::exit(0);
			}
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
						other => return Err(format!("unknown approval mode '{other}' (terminal|client)")),
					}
			}
			Long("icons") => args.icons = true,
			Long("both") => args.both = true,
			Long("mirror") => args.mirror = true,
			Long("apply") => args.do_apply = true,
			Long("json") => args.json = true,
			Long("max-ops") => {
				let v = parser.value().map_err(|e| e.to_string())?.string().map_err(|e| e.to_string())?;
				args.max_ops = Some(v.parse().map_err(|_| format!("--max-ops needs a number, got '{v}'"))?);
			}
			Long("home") => args.home = Some(parser.value().map_err(|e| e.to_string())?.into()),
			Long("source") => {
				args.source =
					match parser.value().map_err(|e| e.to_string())?.string().map_err(|e| e.to_string())?.as_str() {
						"human" => Source::Human,
						"agent" => Source::Agent,
						other => return Err(format!("unknown source '{other}' (human|agent)")),
					}
			}
			Value(v) if args.command.is_empty() => args.command = v.string().map_err(|e| e.to_string())?,
			Value(v) if args.plan.is_none() => args.plan = Some(v),
			Value(v) if args.extra.is_none() => args.extra = Some(v),
			other => return Err(other.unexpected().to_string()),
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
	if args.command.is_empty() {
		if io::stdin().is_terminal() && io::stdout().is_terminal() {
			args.command = "ui".into();
		} else {
			return Err(format!("missing command\n\n{USAGE}"));
		}
	}
	Ok(args)
}

fn run(args: Args) -> Result<ExitCode, String> {
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
			let validator = any_validator()?;
			match Executor::new(&validator, &mut journal, trash).undo_last() {
				Ok(Some(b)) => println!("Undone batch {} ({} step(s)).", b.id, b.steps),
				Ok(None) => println!("Nothing to undo."),
				Err(e) => return Err(e.to_string()),
			}
			Ok(ExitCode::SUCCESS)
		}
		"log" => {
			let (mut journal, _) = open_state(&args)?;
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
				};
				println!("{}  {:<6} {:<10} {} step(s)  {}", b.id, b.source, status, b.steps.len(), b.reason);
			}
			if journal.corrupt_lines() > 0 {
				eprintln!("warning: {} unreadable journal line(s) skipped", journal.corrupt_lines());
			}
			Ok(ExitCode::SUCCESS)
		}
		"recover" => {
			let (mut journal, trash) = open_state(&args)?;
			let validator = any_validator()?;
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
				return Err("at least one --root <dir> is required".into());
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
		other => Err(format!("unknown command '{other}'\n\n{USAGE}")),
	}
}

fn check(args: &Args, apply: bool) -> Result<ExitCode, String> {
	let plan = read_plan(args.plan.as_deref().ok_or("missing plan file (use - for stdin)")?)?;
	if args.roots.is_empty() {
		return Err("at least one --root <dir> is required".into());
	}
	let mut validator = Validator::new(&args.roots).map_err(|e| format!("invalid root: {e}"))?;
	if let Some(n) = args.max_ops {
		// A big plan you made on purpose (a photo clean-up); agents keep the default limit.
		if args.source == Source::Agent {
			return Err("--max-ops is for human plans only".into());
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

fn execute(args: &Args, validator: &Validator, plan: &Plan) -> Result<ExitCode, String> {
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
		Err(e) => Err(e.to_string()),
	}
}

/// `tpfs sync <source> <target>`: compare, build a plan of copy/trash, show it; apply only with
/// `--apply` after confirmation (or `--approve <digest>` from a previous dry run).
fn sync(args: &Args) -> Result<ExitCode, String> {
	use tpfs_core::sync::{Direction, SyncOptions, compare, sync_plan};

	let dir = |v: &Option<OsString>, what: &str| -> Result<PathBuf, String> {
		let v = v.as_deref().ok_or(format!("missing {what} folder: tpfs sync <source> <target>"))?;
		let p = tpfs_core::canonicalize(Path::new(v)).map_err(|e| format!("{}: {e}", Path::new(v).display()))?;
		if p.is_dir() { Ok(p) } else { Err(format!("not a folder: {}", p.display())) }
	};
	let (source, target) = (dir(&args.plan, "source")?, dir(&args.extra, "target")?);
	if source.starts_with(&target) || target.starts_with(&source) {
		return Err("source and target must be separate folders (one is inside the other)".into());
	}
	if args.both && args.mirror {
		return Err("--mirror only works one way; drop --both or --mirror".into());
	}

	let cmp = compare(&source, &target, true).map_err(|e| e.to_string())?;
	if cmp.truncated {
		return Err(format!("too many files to compare (limit {})", tpfs_core::sync::MAX_ENTRIES));
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

	let validator = Validator::new([&source, &target]).map_err(|e| e.to_string())?;
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
fn confirm(args: &Args, digest: &str, level: Level) -> Result<(), String> {
	if let Some(approved) = &args.approve {
		return if approved == digest {
			Ok(())
		} else {
			Err("--approve does not match this plan's digest; nothing was changed".into())
		};
	}
	if args.source == Source::Agent {
		return Err("plans from agents need a human approval: --approve <digest>".into());
	}
	if !io::stdin().is_terminal() || args.plan.as_deref() == Some("-".as_ref()) {
		return Err("not interactive: pass --approve <digest> to apply exactly this plan".into());
	}

	let prompt = if level == Level::High { "Type 'yes' to apply this HIGH-risk plan: " } else { "Apply? [y/N] " };
	print!("{prompt}");
	io::stdout().flush().ok();
	let mut answer = String::new();
	io::stdin().lock().read_line(&mut answer).map_err(|e| e.to_string())?;
	let answer = answer.trim();
	let ok = if level == Level::High { answer == "yes" } else { matches!(answer, "y" | "Y" | "yes") };
	if ok { Ok(()) } else { Err("cancelled; nothing was changed".into()) }
}

/// A human reviews an agent's plan in their own terminal and approves exactly that plan.
/// Deliberately interactive only: there is no flag an agent could pass to approve its own plan.
fn approve(args: &Args) -> Result<ExitCode, String> {
	let id = plan_id(args)?;
	let store = tpfs_mcp::Store::new(&state_dir(args)?);
	let pending = store.get(&id).map_err(|e| e.to_string())?;
	if pending.plan.digest() != pending.digest {
		return Err("the stored plan was modified after it was proposed; reject it".into());
	}

	let validator = Validator::new(&pending.roots).map_err(|e| format!("invalid root: {e}"))?;
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

	if !io::stdin().is_terminal() {
		return Err("approval must be given interactively in a terminal".into());
	}
	print!("Type 'yes' to let the agent apply exactly this plan: ");
	io::stdout().flush().ok();
	let mut answer = String::new();
	io::stdin().lock().read_line(&mut answer).map_err(|e| e.to_string())?;
	if answer.trim() != "yes" {
		return Err("not approved".into());
	}
	store.approve(&pending).map_err(|e| e.to_string())?;
	println!("Approved. The agent can now call apply_plan for {id}.");
	Ok(ExitCode::SUCCESS)
}

fn plan_id(args: &Args) -> Result<String, String> {
	args.plan.as_ref().and_then(|p| p.to_str()).map(str::to_owned).ok_or_else(|| "missing <planId>".to_owned())
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

/// Undo and recovery replay inverses recorded in the journal; they are not confined to roots.
fn any_validator() -> Result<Validator, String> {
	Validator::new(tpfs_core::system_roots()).map_err(|e| e.to_string())
}
