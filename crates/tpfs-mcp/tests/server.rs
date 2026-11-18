use std::{fs, path::PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;
use tpfs_core::{Executor, Journal, Op, Plan, Source, Validator};
use tpfs_mcp::{Approval, Config, Server};

struct Fixture {
	_tmp: TempDir,
	root: PathBuf,
	state: PathBuf,
	server: Server,
	next: u64,
}

impl Fixture {
	fn new(approval: Approval) -> Self {
		let tmp = TempDir::new().unwrap();
		let base = fs::canonicalize(tmp.path()).unwrap();
		let root = base.join("root");
		let state = base.join("state");
		fs::create_dir(&root).unwrap();
		let server = Server::new(Config { roots: vec![root.clone()], state: state.clone(), approval }).unwrap();
		Self { _tmp: tmp, root, state, server, next: 1 }
	}

	fn rpc(&mut self, method: &str, params: Value) -> Value {
		self.next += 1;
		let msg = json!({ "jsonrpc": "2.0", "id": self.next, "method": method, "params": params });
		self.server.handle(&msg.to_string()).expect("a request gets a response")
	}

	/// Call a tool; returns (payload, is_error).
	fn tool(&mut self, name: &str, arguments: Value) -> (Value, bool) {
		let r = self.rpc("tools/call", json!({ "name": name, "arguments": arguments }));
		let result = &r["result"];
		let text = result["content"][0]["text"].as_str().unwrap();
		(serde_json::from_str(text).unwrap(), result["isError"].as_bool().unwrap())
	}

	fn propose(&mut self, plan: Value) -> String {
		let (v, err) = self.tool("propose_plan", json!({ "plan": plan }));
		assert!(!err && v["valid"] == true, "{v}");
		v["planId"].as_str().unwrap().to_owned()
	}
}

#[test]
fn handshake_and_tool_list() {
	let mut f = Fixture::new(Approval::Terminal);
	let init = f.rpc("initialize", json!({ "protocolVersion": "2025-06-18" }));
	assert_eq!(init["result"]["serverInfo"]["name"], "tpfs");
	assert!(init["result"]["instructions"].as_str().unwrap().contains("tpfs approve"));
	assert!(f.server.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());

	let tools = f.rpc("tools/list", json!({}));
	let names: Vec<&str> =
		tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
	assert_eq!(names, ["list_directory", "propose_plan", "apply_plan", "undo_last", "history"]);

	// The plan schema's definitions are hoisted so that $ref pointers resolve from the root.
	let propose = &tools["result"]["tools"][1]["inputSchema"];
	assert!(propose["$defs"].is_object(), "{propose}");
	assert!(propose["properties"]["plan"].get("$defs").is_none());

	assert_eq!(f.rpc("nope", json!({}))["error"]["code"], -32601);
}

#[test]
fn agent_cannot_apply_without_human_approval() {
	let mut f = Fixture::new(Approval::Terminal);
	let a = f.root.join("a.txt");
	fs::write(&a, "A").unwrap();
	let id = f.propose(json!({ "reason": "tidy", "ops": [{ "op": "trash", "path": a }] }));

	let (v, err) = f.tool("apply_plan", json!({ "planId": id }));
	assert!(err && v["error"].as_str().unwrap().contains("tpfs approve"), "{v}");
	assert!(a.exists(), "applied without approval");

	// The human approves in their terminal (here: through the same store the CLI uses).
	let store = f.server.store().clone();
	store.approve(&store.get(&id).unwrap()).unwrap();

	let (v, err) = f.tool("apply_plan", json!({ "planId": id }));
	assert!(!err, "{v}");
	assert!(!a.exists());

	// A plan is applied once.
	let (_, err) = f.tool("apply_plan", json!({ "planId": id }));
	assert!(err);
}

#[test]
fn approval_is_void_if_the_plan_is_modified() {
	let mut f = Fixture::new(Approval::Terminal);
	let a = f.root.join("a.txt");
	let b = f.root.join("b.txt");
	fs::write(&a, "A").unwrap();
	fs::write(&b, "B").unwrap();
	let id = f.propose(json!({ "ops": [{ "op": "trash", "path": a }] }));
	let store = f.server.store().clone();
	store.approve(&store.get(&id).unwrap()).unwrap();

	// Something rewrites the pending plan after approval to trash a different file.
	let file = f.state.join("pending").join(format!("{id}.json"));
	let tampered = fs::read_to_string(&file).unwrap().replace("a.txt", "b.txt");
	fs::write(&file, tampered).unwrap();

	let (v, err) = f.tool("apply_plan", json!({ "planId": id }));
	assert!(err, "{v}");
	assert!(a.exists() && b.exists());
}

#[test]
fn client_mode_trusts_the_clients_confirmation() {
	let mut f = Fixture::new(Approval::Client);
	let dir = f.root.join("new");
	let id = f.propose(json!({ "ops": [{ "op": "mkdir", "path": dir }] }));
	let (v, err) = f.tool("apply_plan", json!({ "planId": id }));
	assert!(!err, "{v}");
	assert!(dir.is_dir());
}

#[test]
fn invalid_plans_and_paths_are_reported_not_stored() {
	let mut f = Fixture::new(Approval::Terminal);
	let (v, err) = f.tool("propose_plan", json!({ "plan": { "ops": [{ "op": "trash", "path": "/etc/passwd" }] } }));
	assert!(!err && v["valid"] == false, "{v}");
	assert!(v["issues"][0].as_str().unwrap().contains("Outside"));

	let (v, err) = f.tool("list_directory", json!({ "path": "/" }));
	assert!(err, "{v}");

	let (v, err) = f.tool("apply_plan", json!({ "planId": "../../journal" }));
	assert!(err && v["error"].as_str().unwrap().contains("invalid plan id"), "{v}");
}

#[test]
fn listing_sanitises_hostile_names() {
	let mut f = Fixture::new(Approval::Terminal);
	fs::write(f.root.join("evil\x1b[2Jname"), "").unwrap();
	fs::create_dir(f.root.join("sub")).unwrap();
	let (v, err) = f.tool("list_directory", json!({ "path": f.root }));
	assert!(!err, "{v}");
	assert_eq!(v["entries"][0]["type"], "dir");
	let name = v["entries"][1]["name"].as_str().unwrap();
	assert!(!name.contains('\x1b') && name.contains("\\u{1b}"), "{name}");
}

#[test]
fn agent_undoes_only_agent_batches() {
	let mut f = Fixture::new(Approval::Client);
	let dir = f.root.join("by-agent");
	let id = f.propose(json!({ "ops": [{ "op": "mkdir", "path": dir }] }));
	f.tool("apply_plan", json!({ "planId": id }));

	// A human makes a change afterwards.
	let human_dir = f.root.join("by-human");
	{
		let v = Validator::new([&f.root]).unwrap();
		let mut j = Journal::open(f.state.join("journal.jsonl")).unwrap();
		Executor::new(&v, &mut j, f.state.join("trash"))
			.execute(&Plan::new(vec![Op::Mkdir { path: human_dir.clone() }], "human"), Source::Human)
			.unwrap();
	}

	let (v, err) = f.tool("undo_last", json!({}));
	assert!(err && v["error"].as_str().unwrap().contains("human"), "{v}");
	assert!(human_dir.exists());

	let (v, _) = f.tool("history", json!({}));
	assert_eq!(v["batches"][0]["source"], "human");
	assert_eq!(v["batches"][1]["source"], "agent");
}
