use std::{
	fs,
	io::{self, BufRead, Write},
	path::{Path, PathBuf},
};

use planfs_core::{BatchStatus, Executor, Journal, Plan, Source, Validator, risk, sanitize};
use serde_json::{Value, json};

use crate::store::Store;

pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Who confirms an agent's plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
	/// A human runs `planfs approve <id>` in a terminal. The default: a channel the agent does not drive.
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
}

impl Server {
	pub fn new(config: Config) -> io::Result<Self> {
		let validator = Validator::new(&config.roots)?;
		let store = Store::new(&config.state);
		Ok(Self { config, validator, store })
	}

	pub fn store(&self) -> &Store {
		&self.store
	}

	/// Serve newline-delimited JSON-RPC until the input closes. Nothing but protocol goes to `output`.
	pub fn run(&mut self, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
		for line in input.lines() {
			let line = line?;
			if line.trim().is_empty() {
				continue;
			}
			if let Some(response) = self.handle(&line) {
				writeln!(output, "{response}")?;
				output.flush()?;
			}
		}
		Ok(())
	}

	/// Handle one JSON-RPC message; notifications get no response.
	pub fn handle(&mut self, line: &str) -> Option<Value> {
		let message: Value = match serde_json::from_str(line) {
			Ok(v) => v,
			Err(e) => return Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
		};
		let id = message.get("id").cloned();
		let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
		let params = message.get("params").cloned().unwrap_or(Value::Null);

		let id = id?; // notifications (e.g. notifications/initialized) are not answered
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

	fn initialize(&self, params: &Value) -> Value {
		let version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
		let roots: Vec<String> = self.validator.roots().iter().map(|r| show(r)).collect();
		let approval = match self.config.approval {
			Approval::Terminal => {
				"After proposing, ask the user to review and approve it by running `planfs approve <planId>` in their terminal, then call apply_plan."
			}
			Approval::Client => {
				"apply_plan asks the user for confirmation through this client; show them the preview first."
			}
		};
		json!({
			"protocolVersion": version,
			"capabilities": { "tools": {} },
			"serverInfo": { "name": "planfs", "version": env!("CARGO_PKG_VERSION") },
			"instructions": format!(
				"Safe, undoable file operations. You cannot change files directly: describe the change as a plan \
				 with propose_plan, show the user the returned preview and risk, and wait for their approval. {approval} \
				 Nothing is ever overwritten or deleted permanently ('trash' is undoable). Allowed folders: {}.",
				roots.join(", ")
			),
		})
	}

	fn call_tool(&mut self, params: &Value) -> Result<Value, (i64, String)> {
		let name = params.get("name").and_then(Value::as_str).ok_or((-32602, "missing tool name".to_owned()))?;
		let args = params.get("arguments").cloned().unwrap_or(json!({}));
		let outcome = match name {
			"list_directory" => self.list_directory(&args),
			"propose_plan" => self.propose_plan(&args),
			"apply_plan" => self.apply_plan(&args),
			"undo_last" => self.undo_last(),
			"history" => self.history(),
			_ => return Err((-32602, format!("unknown tool: {name}"))),
		};
		Ok(match outcome {
			Ok(v) => tool_result(&v, false),
			Err(e) => tool_result(&json!({ "error": e }), true),
		})
	}

	fn list_directory(&self, args: &Value) -> Result<Value, String> {
		let path = args.get("path").and_then(Value::as_str).ok_or("'path' is required")?;
		let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(500).clamp(1, 5000) as usize;
		let dir = self.validator.check_path(Path::new(path))?;

		let mut entries: Vec<(bool, String, u64)> = fs::read_dir(&dir)
			.map_err(|e| format!("{}: {e}", show(&dir)))?
			.filter_map(Result::ok)
			.map(|e| {
				let meta = fs::symlink_metadata(e.path()).ok();
				let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
				(is_dir, sanitize(&e.file_name()), meta.map_or(0, |m| m.len()))
			})
			.collect();
		entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));

		let total = entries.len();
		let listed: Vec<Value> = entries
			.into_iter()
			.take(limit)
			.map(|(d, name, size)| {
				if d {
					json!({ "name": name, "type": "dir" })
				} else {
					json!({ "name": name, "type": "file", "size": size })
				}
			})
			.collect();
		Ok(json!({ "path": show(&dir), "total": total, "truncated": total > limit, "entries": listed }))
	}

	fn propose_plan(&mut self, args: &Value) -> Result<Value, String> {
		let plan: Plan = serde_json::from_value(args.get("plan").cloned().ok_or("'plan' is required")?)
			.map_err(|e| format!("not a valid plan: {e}"))?;

		let issues = self.validator.validate(&plan);
		let risk = risk::assess(&plan, Source::Agent);
		let preview = plan.preview();
		if !issues.is_empty() {
			return Ok(json!({
				"valid": false,
				"issues": issues.iter().map(ToString::to_string).collect::<Vec<_>>(),
				"preview": preview,
			}));
		}

		let pending = self.store.put(&plan, self.validator.roots()).map_err(|e| e.to_string())?;
		let next = match self.config.approval {
			Approval::Terminal => format!(
				"Show the user the preview and ask them to run `planfs approve {}` in a terminal. Then call apply_plan.",
				pending.id
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
			"preview": preview,
			"next": next,
		}))
	}

	fn apply_plan(&mut self, args: &Value) -> Result<Value, String> {
		let id = args.get("planId").and_then(Value::as_str).ok_or("'planId' is required")?;
		let pending = self.store.get(id).map_err(|e| e.to_string())?;

		if pending.plan.digest() != pending.digest {
			self.store.remove(id).ok();
			return Err("the stored plan was modified after it was proposed; propose it again".into());
		}
		if self.config.approval == Approval::Terminal && !self.store.is_approved(&pending) {
			return Err(format!(
				"not approved yet. Ask the user to run `planfs approve {id}` in a terminal, then call apply_plan again."
			));
		}

		let mut journal = self.journal()?;
		let result = Executor::new(&self.validator, &mut journal, self.config.state.join("trash"))
			.execute(&pending.plan, Source::Agent);
		self.store.remove(id).ok();
		let batch = result.map_err(|e| e.to_string())?;
		Ok(
			json!({ "applied": batch.steps, "batch": batch.id, "undo": "undo_last reverts it; the user can also run `planfs undo`." }),
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
				return Err("the most recent batch was made by a human; only they can undo it (`planfs undo`)".into());
			}
			Some(_) => {}
		}
		let undo_all = Validator::new(planfs_core::system_roots()).map_err(|e| e.to_string())?;
		let report = Executor::new(&undo_all, &mut journal, self.config.state.join("trash"))
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
		let recent: Vec<Value> = batches
			.iter()
			.rev()
			.take(20)
			.map(|b| json!({ "batch": b.id, "source": b.source, "status": format!("{:?}", b.status), "steps": b.steps.len(), "reason": sanitize(b.reason.as_ref()) }))
			.collect();
		Ok(json!({ "batches": recent }))
	}

	/// The journal is opened per call, so `planfs undo` and friends keep working while the server runs.
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
	});
	if let Some(defs) = defs {
		propose_schema["$defs"] = defs; // references like #/$defs/Op resolve against the root
	}

	json!([
		{
			"name": "list_directory",
			"description": "List a directory inside the allowed folders (names, types, sizes).",
			"inputSchema": {
				"type": "object",
				"properties": {
					"path": { "type": "string", "description": "Absolute path." },
					"limit": { "type": "integer", "description": "Maximum entries (default 500)." }
				},
				"required": ["path"]
			},
			"annotations": { "readOnlyHint": true, "openWorldHint": false }
		},
		{
			"name": "propose_plan",
			"description": "Validate a plan of file operations WITHOUT executing it. Returns a preview, the risk, and a planId. Paths are absolute; for move/copy 'to' is the full destination path. There is no permanent delete: use 'trash'.",
			"inputSchema": propose_schema,
			"annotations": { "readOnlyHint": true, "openWorldHint": false }
		},
		{
			"name": "apply_plan",
			"description": "Execute a proposed plan after the user approved it. Fails if it was not approved or was modified.",
			"inputSchema": {
				"type": "object",
				"properties": { "planId": { "type": "string" } },
				"required": ["planId"]
			},
			"annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
		},
		{
			"name": "undo_last",
			"description": "Revert the most recent batch, if it was made by an agent.",
			"inputSchema": { "type": "object", "properties": {} },
			"annotations": { "readOnlyHint": false, "destructiveHint": true, "openWorldHint": false }
		},
		{
			"name": "history",
			"description": "Recent batches: who made them, status, number of steps.",
			"inputSchema": { "type": "object", "properties": {} },
			"annotations": { "readOnlyHint": true, "openWorldHint": false }
		}
	])
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
