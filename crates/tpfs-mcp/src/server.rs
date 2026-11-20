use std::{
	fs,
	io::{self, BufRead, Read, Write},
	path::{Path, PathBuf},
};

use serde_json::{Value, json};
use tpfs_core::{BatchStatus, Executor, Journal, Plan, Source, Validator, risk, sanitize};

use crate::{recipes::RecipeStore, store::Store};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Protocol versions this server implements; a client asking for another gets [`PROTOCOL_VERSION`].
const SUPPORTED_VERSIONS: [&str; 3] = [PROTOCOL_VERSION, "2025-03-26", "2024-11-05"];

/// Longest plan reason accepted from an agent: stricter than the core limit, as the reason is
/// shown to the human approving it and should stay readable.
const AGENT_MAX_REASON: usize = 2_000;
/// Preview lines returned for a valid plan; the rest is counted in `stepCount`.
const PREVIEW_LINES: usize = 200;

/// Longest JSON-RPC message accepted on stdin.
pub const MAX_MESSAGE: usize = 4 << 20;

/// Who confirms an agent's plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
	/// A human runs `tpfs approve <id>` in a terminal. The default: a channel the agent does not drive.
	Terminal,
	/// Trust the MCP client's own confirmation prompt for the `apply_plan` tool call.
	/// Weaker: an "always allow" setting in the client turns it into no approval at all.
	Client,
}

#[derive(Clone, Debug)]
pub struct Config {
	pub roots: Vec<PathBuf>,
	pub state: PathBuf,
	pub approval: Approval,
}

pub struct Server {
	config: Config,
	validator: Validator,
	store: Store,
	recipes: RecipeStore,
	/// The client's name for itself, from `initialize` (not verified).
	client: String,
}

impl Server {
	pub fn new(config: Config) -> io::Result<Self> {
		let validator = Validator::new(&config.roots)?.with_protected([&config.state]);
		let store = Store::new(&config.state);
		let recipes = RecipeStore::new(&config.state);
		Ok(Self { config, validator, store, recipes, client: String::new() })
	}

	pub fn store(&self) -> &Store {
		&self.store
	}

	pub fn recipes(&self) -> &RecipeStore {
		&self.recipes
	}

	/// Serve newline-delimited JSON-RPC until the input closes. Nothing but protocol goes to `output`.
	/// A message that is not valid UTF-8 or longer than [`MAX_MESSAGE`] is answered with a JSON-RPC
	/// error and skipped; it never ends the session.
	pub fn run(&mut self, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
		let mut buf = Vec::new();
		loop {
			buf.clear();
			let n = (&mut input).take(MAX_MESSAGE as u64 + 1).read_until(b'\n', &mut buf)?;
			if n == 0 {
				return Ok(());
			}
			let response = if buf.last() != Some(&b'\n') && buf.len() > MAX_MESSAGE {
				// Skip the rest of the oversized line without buffering it.
				loop {
					buf.clear();
					let m = (&mut input).take(MAX_MESSAGE as u64).read_until(b'\n', &mut buf)?;
					if m == 0 || buf.last() == Some(&b'\n') {
						break;
					}
				}
				Some(rpc_error(Value::Null, -32600, &format!("message longer than {MAX_MESSAGE} bytes")))
			} else {
				match std::str::from_utf8(&buf) {
					Ok(line) if line.trim().is_empty() => None,
					Ok(line) => self.handle(line),
					Err(_) => Some(rpc_error(Value::Null, -32700, "parse error: message is not valid UTF-8")),
				}
			};
			if let Some(response) = response {
				writeln!(output, "{response}")?;
				output.flush()?;
			}
		}
	}

	/// Handle one JSON-RPC message; notifications get no response.
	pub fn handle(&mut self, line: &str) -> Option<Value> {
		let message: Value = match serde_json::from_str(line) {
			Ok(v) => v,
			Err(e) => return Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
		};
		if !message.is_object() {
			// Batches are not supported (MCP 2025-06-18 removed them).
			return Some(rpc_error(Value::Null, -32600, "invalid request: expected a JSON-RPC object"));
		}
		let id = message.get("id").cloned();
		let Some(method) = message.get("method").and_then(Value::as_str) else {
			// A response from the client (result/error) is never answered; anything else is invalid.
			return match id {
				Some(_) if message.get("result").is_some() || message.get("error").is_some() => None,
				Some(id) => Some(rpc_error(id, -32600, "invalid request: missing 'method'")),
				None => None,
			};
		};
		let params = message.get("params").cloned().unwrap_or(Value::Null);

		let id = id?; // notifications (e.g. notifications/initialized) are not answered
		if id.is_null() || !(id.is_string() || id.is_number()) {
			return Some(rpc_error(Value::Null, -32600, "invalid request: 'id' must be a string or a number"));
		}
		if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
			return Some(rpc_error(id, -32600, "invalid request: 'jsonrpc' must be \"2.0\""));
		}
		let result = match method {
			"initialize" => Ok(self.initialize(&params)),
			"ping" => Ok(json!({})),
			"tools/list" => Ok(json!({ "tools": tools() })),
			"tools/call" => self.call_tool(&params),
			_ => Err((-32601, format!("method not found: {method}"))),
		};
		Some(match result {
			Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
			Err((code, msg)) => rpc_error(id, code, &msg),
		})
	}

	/// The exact command the human runs: with `--home` naming this server's state folder, so it
	/// finds the plan whatever TPFS_HOME or --home the server was started with.
	fn approve_command(&self, id: &str) -> String {
		// The real path when it is printable as it is; otherwise the sanitised one, which cannot
		// be pasted as it stands (rare: control characters or invalid Unicode in the path).
		let raw = self.config.state.to_string_lossy();
		let home = if show(&self.config.state) == raw { shell_quote(&raw) } else { show(&self.config.state) };
		format!("tpfs approve {id} --home {home}")
	}

	fn initialize(&mut self, params: &Value) -> Value {
		let name = params.pointer("/clientInfo/name").and_then(Value::as_str).unwrap_or("unknown");
		self.client = sanitize(name.chars().take(64).collect::<String>().as_ref());
		let asked = params.get("protocolVersion").and_then(Value::as_str);
		let version = asked.filter(|v| SUPPORTED_VERSIONS.contains(v)).unwrap_or(PROTOCOL_VERSION);
		let roots: Vec<String> = self.validator.roots().iter().map(|r| show(r)).collect();
		let approval = match self.config.approval {
			Approval::Terminal => format!(
				"After proposing, ask the user to review and approve it by running `{}` in their terminal{}, then call apply_plan.",
				self.approve_command("<planId>"),
				SHELL
			),
			Approval::Client => {
				"apply_plan asks the user for confirmation through this client; show them the preview first.".to_owned()
			}
		};
		json!({
			"protocolVersion": version,
			"capabilities": { "tools": {} },
			"serverInfo": { "name": "tpfs", "version": env!("CARGO_PKG_VERSION") },
			"instructions": format!(
				"Safe, undoable file operations. You cannot change files directly: describe the change as a plan \
				 with propose_plan, show the user the returned preview and risk, and wait for their approval. {approval} \
				 Nothing is ever overwritten or deleted permanently ('trash' is undoable). Allowed folders: {}. \
				 For a task the user repeats, you can propose a recipe (recipe_schema, recipe_validate, recipe_propose): \
				 the user installs it in their commander and runs it on their own selection, without you.",
				roots.join(", ")
			),
		})
	}

	fn call_tool(&mut self, params: &Value) -> Result<Value, (i64, String)> {
		let name = params.get("name").and_then(Value::as_str).ok_or((-32602, "missing tool name".to_owned()))?;
		// A missing or null `arguments` means "no arguments" (some clients send null).
		let args = match params.get("arguments") {
			None | Some(Value::Null) => json!({}),
			Some(a) => a.clone(),
		};
		let allowed: &[&str] = match name {
			"list_directory" => &["path", "limit"],
			"propose_plan" => &["plan"],
			"apply_plan" => &["planId"],
			"undo_last" | "history" | "recipe_schema" => &[],
			"recipe_validate" => &["recipe", "sample"],
			"recipe_propose" => &["recipe", "note"],
			"recipe_status" => &["id"],
			_ => return Err((-32602, format!("unknown tool: {name}"))),
		};
		let Some(obj) = args.as_object() else {
			return Ok(tool_result(&json!({ "error": "'arguments' must be an object" }), true));
		};
		if let Some(extra) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
			let msg = if allowed.is_empty() {
				format!("unknown argument '{extra}': {name} takes no arguments")
			} else {
				format!("unknown argument '{extra}' for {name} (it takes: {})", allowed.join(", "))
			};
			return Ok(tool_result(&json!({ "error": msg }), true));
		}
		let outcome = match name {
			"list_directory" => self.list_directory(&args),
			"propose_plan" => self.propose_plan(&args),
			"apply_plan" => self.apply_plan(&args),
			"undo_last" => self.undo_last(),
			"history" => self.history(),
			"recipe_schema" => Ok(recipe_schema()),
			"recipe_validate" => recipe_validate(&args),
			"recipe_propose" => self.recipe_propose(&args),
			"recipe_status" => self.recipe_status(&args),
			_ => return Err((-32602, format!("unknown tool: {name}"))),
		};
		Ok(match outcome {
			// An invalid plan is reported as a tool error so every client shows it to the model.
			Ok(v) => {
				let invalid = v.get("valid") == Some(&Value::Bool(false));
				tool_result(&v, invalid)
			}
			Err(e) => tool_result(&json!({ "error": e }), true),
		})
	}

	fn list_directory(&self, args: &Value) -> Result<Value, String> {
		let path = args.get("path").and_then(Value::as_str).ok_or("'path' is required")?;
		let limit = match args.get("limit") {
			None | Some(Value::Null) => 500,
			// 5.0 is an integer in JSON Schema terms; some clients send numbers as floats.
			Some(v) => {
				match v.as_u64().or_else(|| v.as_f64().filter(|f| f.fract() == 0.0 && *f >= 0.0).map(|f| f as u64)) {
					Some(n @ 1..=5000) => n as usize,
					_ => return Err("'limit' must be an integer from 1 to 5000".into()),
				}
			}
		};
		let dir = self.validator.check_path(Path::new(path))?;
		// check_path allows a symlink as the last component (operations act on the link itself);
		// listing would follow it out of the allowed folders.
		if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
			return Err(format!("{} is a symlink; list the folder it points to only if that is allowed", show(&dir)));
		}

		// Keep at most a bounded number of entries in memory, however big the folder is.
		const SCAN: usize = 100_000;
		let mut entries: Vec<(&str, String, u64)> = Vec::new();
		// Listed relative to the folder's own confined handle: a folder swapped for a link after the
		// check above is refused instead of followed out of the allowed folders.
		let (listing, all) = tpfs_core::list_confined(self.validator.roots(), &dir, SCAN)
			.map_err(|e| format!("{}: {}", show(&dir), sanitize(e.to_string().as_ref())))?;
		let mut total = all;
		for e in listing {
			// tpfs's own working entries (local trash, partial copies) are not the agent's business.
			if tpfs_core::is_own_name(&e.name) {
				total -= 1;
				continue;
			}
			let kind = if e.dir {
				"dir"
			} else if e.link {
				"symlink"
			} else {
				"file"
			};
			entries.push((kind, sanitize(&e.name), e.size));
		}
		entries.sort_by_cached_key(|e| (e.0 != "dir", e.1.to_lowercase()));

		let listed: Vec<Value> = entries
			.into_iter()
			.take(limit)
			.map(|(kind, name, size)| match kind {
				"file" => json!({ "name": name, "type": "file", "size": size }),
				_ => json!({ "name": name, "type": kind }),
			})
			.collect();
		Ok(json!({ "path": show(&dir), "total": total, "truncated": total > limit, "entries": listed }))
	}

	fn propose_plan(&mut self, args: &Value) -> Result<Value, String> {
		let plan: Plan = serde_json::from_value(args.get("plan").cloned().ok_or("'plan' is required")?)
			.map_err(|e| format!("not a valid plan: {}", sanitize(e.to_string().as_ref())))?;
		if plan.reason.chars().count() > AGENT_MAX_REASON {
			return Err(format!("the reason is too long (limit {AGENT_MAX_REASON} characters)"));
		}

		let issues = self.validator.validate(&plan);
		let risk = risk::assess(&plan, Source::Agent);
		let preview = plan.preview();
		if !issues.is_empty() {
			// An error result, so clients surface it; the preview is cut short to spare context.
			let shown: Vec<&String> = preview.iter().take(20).collect();
			let all = issues.len();
			let issues: Vec<String> = issues.iter().take(50).map(ToString::to_string).collect();
			return Ok(json!({
				"valid": false,
				"issues": issues,
				"issuesTruncated": all > 50,
				"issueCount": all,
				"preview": shown,
				"previewTruncated": preview.len() > 20,
				"stepCount": plan.ops.len(),
			}));
		}

		let pending = self.store.put(&plan, self.validator.roots()).map_err(|e| e.to_string())?;
		let next = match self.config.approval {
			Approval::Terminal => format!(
				"Show the user the preview and ask them to run `{}` in a terminal{} (or to answer it in `tpfs watch`, if it is running). Then call apply_plan.",
				self.approve_command(&pending.id),
				SHELL
			),
			Approval::Client => {
				"Show the user the preview, then call apply_plan; the client will ask them to confirm.".to_owned()
			}
		};
		Ok(json!({
			"valid": true,
			"planId": pending.id,
			"digest": pending.digest,
			"risk": format!("{:?}", risk.level),
			"riskReasons": risk.reasons,
			"previewTruncated": preview.len() > PREVIEW_LINES,
			"preview": preview.iter().take(PREVIEW_LINES).collect::<Vec<_>>(),
			"stepCount": plan.ops.len(),
			"next": next,
		}))
	}

	fn apply_plan(&mut self, args: &Value) -> Result<Value, String> {
		let id = match args.get("planId") {
			Some(Value::String(id)) => id.as_str(),
			Some(_) => return Err("'planId' must be the string id returned by propose_plan".into()),
			None => return Err("'planId' is required".into()),
		};
		let pending = self.store.get(id).map_err(|e| e.to_string())?;

		if pending.plan.digest() != pending.digest {
			self.store.remove(id).ok();
			return Err("the stored plan was modified after it was proposed; propose it again".into());
		}
		if self.config.approval == Approval::Terminal && !self.store.is_approved(&pending) {
			return Err(format!(
				"not approved yet. Ask the user to run `{}` in a terminal{}, then call apply_plan again.",
				self.approve_command(id),
				SHELL
			));
		}

		let mut journal = self.journal()?;
		let result = Executor::new(&self.validator, &mut journal, self.config.state.join("trash"))
			.execute(&pending.plan, Source::Agent);
		// An approval is for one execution: the plan is used up either way.
		let removed = self.store.remove(id);
		let batch = result.map_err(|e| {
			format!("{}\nThe plan was used up; propose a corrected plan if needed.", e.to_string().trim_end())
		})?;
		removed.map_err(|e| {
			format!("applied, but the approved plan could not be removed ({e}); reject it with `tpfs reject {id}`")
		})?;
		Ok(
			json!({ "applied": batch.steps, "batch": batch.id, "undo": "undo_last reverts it; the user can also run `tpfs undo`." }),
		)
	}

	/// Agents may only undo what an agent did. A human's batch is the human's to undo.
	fn undo_last(&mut self) -> Result<Value, String> {
		let mut journal = self.journal()?;
		let last = journal
			.batches()
			.map_err(|e| e.to_string())?
			.into_iter()
			.rev()
			.find(|b| b.status == BatchStatus::Committed);
		match last {
			None => return Ok(json!({ "undone": false, "message": "nothing to undo" })),
			Some(b) if b.source != Source::Agent.as_str() => {
				return Err("the most recent batch was made by a human; only they can undo it (`tpfs undo`)".into());
			}
			Some(_) => {}
		}
		// Confined to the agent's roots: a symlink planted since cannot redirect the undo.
		let report = Executor::new(&self.validator, &mut journal, self.config.state.join("trash"))
			.undo_last()
			.map_err(|e| e.to_string())?;
		Ok(match report {
			Some(b) => json!({ "undone": true, "batch": b.id, "steps": b.steps }),
			None => json!({ "undone": false, "message": "nothing to undo" }),
		})
	}

	fn history(&mut self) -> Result<Value, String> {
		let mut journal = self.journal()?;
		let batches = journal.batches().map_err(|e| e.to_string())?;
		// Only the agent's own batches: human batches name folders outside the agent's roots.
		let recent: Vec<Value> = batches
			.iter()
			.rev()
			.filter(|b| b.source == Source::Agent.as_str())
			.take(20)
			.map(|b| json!({ "batch": b.id, "at": b.at, "source": b.source, "status": status_word(b.status, b.damaged), "steps": b.steps.len(), "reason": sanitize(b.reason.as_ref()) }))
			.collect();
		Ok(json!({ "batches": recent }))
	}

	/// The journal is opened per call, so `tpfs undo` and friends keep working while the server runs.
	fn recipe_propose(&mut self, args: &Value) -> Result<Value, String> {
		let note = match args.get("note") {
			None | Some(Value::Null) => "",
			Some(Value::String(n)) => n.as_str(),
			Some(_) => return Err("'note' must be a string".into()),
		};
		if note.chars().count() > 300 || note.chars().any(|c| c.is_control() && c != '\n') {
			return Err("'note' is at most 300 characters of plain text".into());
		}
		let recipe = match tpfs_recipe::from_value(args.get("recipe").cloned().unwrap_or(Value::Null)) {
			Ok(r) => r,
			Err(errors) => return Ok(json!({ "valid": false, "errors": errors })),
		};
		let pending = self.recipes.put(&recipe, &self.client, note).map_err(|e| e.to_string())?;
		Ok(json!({
			"id": pending.id,
			"status": "pending",
			"next": "Tell the user a recipe is waiting in their commander (Agent recipes). They review and install it, or reject it; recipe_status tells you which. You never run it: the user does, on their own selection.",
		}))
	}

	fn recipe_status(&self, args: &Value) -> Result<Value, String> {
		let Some(id) = args.get("id").and_then(Value::as_str) else {
			return Err("'id' is required: the id returned by recipe_propose".into());
		};
		Ok(json!({ "id": id, "status": self.recipes.status(id).as_str() }))
	}

	fn journal(&self) -> Result<Journal, String> {
		Journal::open(self.config.state.join("journal.jsonl")).map_err(|e| e.to_string())
	}
}

fn tools() -> Value {
	let mut plan_schema = Plan::schema();
	let defs = plan_schema.as_object_mut().and_then(|o| {
		o.remove("$schema");
		o.remove("$defs")
	});
	let mut propose_schema = json!({
		"type": "object",
		"properties": { "plan": plan_schema },
		"required": ["plan"],
		"additionalProperties": false,
	});
	if let Some(p) = propose_schema["properties"]["plan"]["properties"].as_object_mut() {
		if let Some(r) = p.get_mut("reason") {
			r["maxLength"] = json!(AGENT_MAX_REASON);
		}
		if let Some(o) = p.get_mut("ops") {
			o["maxItems"] = json!(tpfs_core::MAX_OPS);
			o["minItems"] = json!(1);
		}
	}
	if let Some(defs) = defs {
		propose_schema["$defs"] = defs; // references like #/$defs/Op resolve against the root
	}

	json!([
		{
			"name": "list_directory",
			"description": "List a directory inside the allowed folders (names, types, sizes). Use it to find the exact absolute paths before proposing a plan.",
			"inputSchema": {
				"type": "object",
				"properties": {
					"path": { "type": "string", "description": "Absolute path." },
					"limit": { "type": "integer", "minimum": 1, "maximum": 5000, "description": "Maximum entries (default 500)." }
				},
				"required": ["path"],
				"additionalProperties": false
			},
			"annotations": { "readOnlyHint": true, "openWorldHint": false }
		},
		{
			"name": "propose_plan",
			"description": "Validate a plan of file operations WITHOUT executing it. Returns a preview, the risk, and a planId. Paths are absolute; for move/copy 'to' is the full destination path. There is no permanent delete: use 'trash'. Nothing changes until a human approves the plan; then call apply_plan with the planId.",
			"inputSchema": propose_schema,
			"annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false }
		},
		{
			"name": "apply_plan",
			"description": "Execute a proposed plan after the user approved it. In terminal mode the user approves with `tpfs approve <planId>`; until then this returns 'not approved yet' and you may call it again later. Fails if the plan was modified. A plan can be applied once; if execution fails it is used up and must be proposed again.",
			"inputSchema": {
				"type": "object",
				"properties": { "planId": { "type": "string", "description": "The planId returned by propose_plan." } },
				"additionalProperties": false,
				"required": ["planId"]
			},
			"annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
		},
		{
			"name": "undo_last",
			"description": "Revert the most recent batch, if it was made by an agent.",
			"inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
			"annotations": { "readOnlyHint": false, "destructiveHint": true, "openWorldHint": false }
		},
		{
			"name": "history",
			"description": "Your recent batches (agent batches only): id, time (Unix seconds), status (committed, rolled back, undone, interrupted = ask the user to run `tpfs recover`, damaged = its journal records are unreadable; ask the user to check `tpfs log`, abandoned = the user gave up recovering it), number of steps, reason.",
			"inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
			"annotations": { "readOnlyHint": true, "openWorldHint": false }
		},
		{
			"name": "recipe_schema",
			"description": "The recipe format (tpfs-recipe/1): JSON Schema, a short guide and two examples. A recipe is a reusable command the user installs in their commander and runs on their own selection; you never run it.",
			"inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
			"annotations": { "readOnlyHint": true, "openWorldHint": false }
		},
		{
			"name": "recipe_validate",
			"description": "Check a recipe without touching any file. With a sample (invented entry names, optional mtimes, and form values) it also returns the steps a run would produce. Errors carry the key they concern.",
			"inputSchema": {
				"type": "object",
				"properties": {
					"recipe": { "type": "object", "description": "The recipe (see recipe_schema)." },
					"sample": {
						"type": "object",
						"properties": {
							"items": { "type": "array", "minItems": 1, "maxItems": 20, "items": { "type": "object", "properties": { "name": { "type": "string" }, "mtime": { "type": "string", "description": "YYYY-MM-DD or YYYY-MM-DDTHH:MM" } }, "required": ["name"], "additionalProperties": false } },
							"values": { "type": "object", "description": "Form field → value." }
						},
						"required": ["items"],
						"additionalProperties": false
					}
				},
				"required": ["recipe"],
				"additionalProperties": false
			},
			"annotations": { "readOnlyHint": true, "openWorldHint": false }
		},
		{
			"name": "recipe_propose",
			"description": "Offer a recipe to the user. It waits in their commander until they install or reject it (at most 5 wait at once). Nothing runs: the user runs an installed recipe on their own selection.",
			"inputSchema": {
				"type": "object",
				"properties": {
					"recipe": { "type": "object", "description": "The recipe (see recipe_schema)." },
					"note": { "type": "string", "maxLength": 300, "description": "Why you propose it, for the user." }
				},
				"required": ["recipe"],
				"additionalProperties": false
			},
			"annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false }
		},
		{
			"name": "recipe_status",
			"description": "Whether a proposed recipe is pending, installed or rejected (unknown: never proposed, or decided over a month ago).",
			"inputSchema": {
				"type": "object",
				"properties": { "id": { "type": "string", "description": "The id returned by recipe_propose." } },
				"required": ["id"],
				"additionalProperties": false
			},
			"annotations": { "readOnlyHint": true, "openWorldHint": false }
		}
	])
}

const RECIPE_GUIDE: &str = "A recipe is data only: a form the user fills in and a target template. \
	The user runs it on entries they select; each run becomes an ordinary plan they review, and can undo. \
	Template: {source[:format][|filter…]} between literal text, '/' between folders. Sources: item.name, item.stem, \
	item.ext, item.mtime, today, n (position in the selection), form.<field>; {item.dir} or a folder field \
	(format x-tpfs-dir) may start a move or copy target, otherwise it is the entry's own folder. Formats: %Y %m %d %H %M \
	for dates, :0N (as :03) for n and integer fields. Filters: lower, trim, ascii (strips diacritics), slug (spaces and \
	unsafe characters to _). A value never creates a folder ('/' in it becomes '_'). Missing folders are created; \
	collisions are numbered (_2, _3 …) with on_conflict number, or stop the run. Every field the template uses must be \
	required. Check with recipe_validate (with a sample of invented names) before recipe_propose.";

fn recipe_schema() -> Value {
	let examples: Vec<Value> = [
		include_str!("../../tpfs-recipe/examples/receipts-to-accounting.json"),
		include_str!("../../tpfs-recipe/examples/photos-by-date.json"),
	]
	.iter()
	.filter_map(|e| serde_json::from_str(e).ok())
	.collect();
	json!({ "format": tpfs_recipe::FORMAT, "schema": tpfs_recipe::Recipe::schema(), "guide": RECIPE_GUIDE, "examples": examples })
}

/// Check a recipe, and expand it over invented entries: no disk access at all (every path is
/// free, the scope is taken as written).
fn recipe_validate(args: &Value) -> Result<Value, String> {
	let recipe = match tpfs_recipe::from_value(args.get("recipe").cloned().unwrap_or(Value::Null)) {
		Ok(r) => r,
		Err(errors) => return Ok(json!({ "valid": false, "errors": errors })),
	};
	let summary = json!({ "id": recipe.id, "version": recipe.version, "title": recipe.title, "hash": recipe.hash() });
	let sample = match args.get("sample") {
		None | Some(Value::Null) => {
			return Ok(
				json!({ "valid": true, "recipe": summary, "next": "Pass a sample (invented names and form values) to see the target names." }),
			);
		}
		Some(s) => s,
	};
	let bad = |m: &str| Err(format!("'sample': {m}"));
	let Some(obj) = sample.as_object() else { return bad("must be an object {items, values}") };
	if let Some(k) = obj.keys().find(|k| !matches!(k.as_str(), "items" | "values")) {
		return bad(&format!("unknown key '{}' (it takes: items, values)", sanitize(k.as_ref())));
	}
	let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
		.map(PathBuf::from)
		.unwrap_or_else(|| PathBuf::from(if cfg!(windows) { r"C:\Users\user" } else { "/home/user" }));
	let roots = recipe.resolve_roots(&home);
	let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
	let today = tpfs_recipe::Stamp::from_unix(i64::try_from(now).unwrap_or(0)).unwrap_or(tpfs_recipe::Stamp {
		year: 2026,
		month: 1,
		day: 1,
		hour: 0,
		minute: 0,
	});
	let list = match obj.get("items") {
		Some(Value::Array(a)) if !a.is_empty() && a.len() <= 20 => a,
		_ => return bad("'items' lists 1–20 invented entries {name, mtime}"),
	};
	let mut items = Vec::new();
	for (i, it) in list.iter().enumerate() {
		let name = it.get("name").and_then(Value::as_str).filter(|n| !n.is_empty() && !n.contains(['/', '\\']));
		let Some(name) = name else { return bad(&format!("items[{i}].name must be a plain file name")) };
		let mtime = match it.get("mtime") {
			None => today,
			Some(Value::String(m)) => match tpfs_recipe::Stamp::parse(m) {
				Some(s) => s,
				None => return bad(&format!("items[{i}].mtime is YYYY-MM-DD or YYYY-MM-DDTHH:MM")),
			},
			Some(_) => return bad(&format!("items[{i}].mtime is a string")),
		};
		if let Some(k) = it.as_object().and_then(|o| o.keys().find(|k| !matches!(k.as_str(), "name" | "mtime"))) {
			return bad(&format!("items[{i}]: unknown key '{}' (it takes: name, mtime)", sanitize(k.as_ref())));
		}
		let dir = recipe.input.kind == tpfs_recipe::Kind::Dir;
		items.push(tpfs_recipe::Item { path: roots[0].join(name), dir, mtime });
	}
	let values = match obj.get("values") {
		None | Some(Value::Null) => tpfs_recipe::Values::default(),
		Some(Value::Object(o)) => tpfs_recipe::Values(o.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
		Some(_) => return bad("'values' is an object: field name → value"),
	};
	match tpfs_recipe::expand(&recipe, &roots, &items, &values, today, &|_| false) {
		Err(errors) => Ok(json!({ "valid": false, "recipe": summary, "errors": errors })),
		Ok(plan) => {
			let steps: Vec<Value> = plan
				.ops
				.iter()
				.map(|op| match op {
					tpfs_core::Op::Mkdir { path } => json!({ "mkdir": show(path) }),
					tpfs_core::Op::Move { from, to } | tpfs_core::Op::Copy { from, to } => {
						json!({ "from": show(from), "to": show(to) })
					}
					tpfs_core::Op::Rename { path, new_name } => {
						json!({ "from": show(path), "newName": sanitize(new_name.as_ref()) })
					}
					_ => json!({}),
				})
				.collect();
			Ok(
				json!({ "valid": true, "recipe": summary, "reason": plan.reason, "steps": steps, "roots": roots.iter().map(|r| show(r)).collect::<Vec<_>>() }),
			)
		}
	}
}

fn status_word(s: BatchStatus, damaged: bool) -> &'static str {
	if damaged {
		return "damaged";
	}
	match s {
		BatchStatus::Committed => "committed",
		BatchStatus::Aborted => "rolled back",
		BatchStatus::Undone => "undone",
		BatchStatus::Incomplete => "interrupted",
		BatchStatus::Abandoned => "abandoned",
		_ => "unknown",
	}
}

fn tool_result(payload: &Value, is_error: bool) -> Value {
	json!({
		"content": [{ "type": "text", "text": serde_json::to_string_pretty(payload).unwrap_or_default() }],
		"isError": is_error,
	})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
	json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn show(p: &Path) -> String {
	sanitize(p.as_os_str())
}

/// Which shell the quoting in approval hints is for.
const SHELL: &str = if cfg!(windows) { " (PowerShell)" } else { "" };

/// A path as one shell word, safe to paste: single quotes on POSIX shells (an inner `'` as `'\''`),
/// PowerShell single quotes on Windows (the hint says the command is for PowerShell).
fn shell_quote(s: &str) -> String {
	if cfg!(windows) {
		// PowerShell (the default Windows terminal): single quotes take everything literally
		// (`$`, backtick, `%`); an inner `'` is doubled.
		// PowerShell also takes the typographic quotes ‘ ’ ‚ ‛ as single quotes.
		// Each is escaped by doubling it.
		let mut out = String::from("'");
		for c in s.chars() {
			if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
				out.push(c);
			}
			out.push(c);
		}
		out.push('\'');
		out
	} else if s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+,:@".contains(&b)) {
		s.to_owned()
	} else {
		format!("'{}'", s.replace('\'', "'\\''"))
	}
}

#[cfg(test)]
mod tests {
	use super::shell_quote;

	#[test]
	fn shell_words_survive_a_paste() {
		if cfg!(windows) {
			assert_eq!(shell_quote(r"C:\Users\x$y\it's"), r"'C:\Users\x$y\it''s'");
			for q in ['\u{2018}', '\u{2019}', '\u{201a}', '\u{201b}'] {
				assert_eq!(shell_quote(&format!("C:\\O{q}Brien")), format!("'C:\\O{q}{q}Brien'"));
			}
		} else {
			assert_eq!(shell_quote("/home/u/tpfs"), "/home/u/tpfs");
			assert_eq!(shell_quote("/x/a b"), "'/x/a b'");
			assert_eq!(shell_quote("/x/it's $HOME"), r#"'/x/it'\''s $HOME'"#);
		}
	}
}
