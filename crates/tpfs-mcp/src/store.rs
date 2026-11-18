use std::{
	fs, io,
	path::{Path, PathBuf},
	time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tpfs_core::Plan;

/// A plan proposed by an agent and waiting for a human decision.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pending {
	pub id: String,
	pub digest: String,
	pub created: u64,
	pub roots: Vec<PathBuf>,
	pub plan: Plan,
}

/// Pending plans and approvals on disk (`<state>/pending`), shared between the MCP server
/// and the `tpfs approve` command running in the user's terminal.
#[derive(Clone, Debug)]
pub struct Store {
	dir: PathBuf,
}

impl Store {
	pub fn new(state: &Path) -> Self {
		Self { dir: state.join("pending") }
	}

	pub fn put(&self, plan: &Plan, roots: &[PathBuf]) -> io::Result<Pending> {
		create_private_dir(&self.dir)?;
		let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
		let pending = Pending {
			id: format!("{:x}{:x}", nanos, std::process::id() & 0xfff),
			digest: plan.digest(),
			created: (nanos / 1_000_000_000) as u64,
			roots: roots.to_vec(),
			plan: plan.clone(),
		};
		write_private(&self.file(&pending.id)?, &serde_json::to_vec_pretty(&pending).map_err(io::Error::other)?)?;
		Ok(pending)
	}

	pub fn get(&self, id: &str) -> io::Result<Pending> {
		let bytes = fs::read(self.file(id)?).map_err(|e| {
			io::Error::new(e.kind(), format!("no pending plan '{id}' (already applied, rejected or never proposed)"))
		})?;
		serde_json::from_slice(&bytes).map_err(io::Error::other)
	}

	pub fn list(&self) -> io::Result<Vec<Pending>> {
		let Ok(entries) = fs::read_dir(&self.dir) else { return Ok(vec![]) };
		let mut out: Vec<Pending> = entries
			.filter_map(Result::ok)
			.filter(|e| e.path().extension().is_some_and(|x| x == "json"))
			.filter_map(|e| fs::read(e.path()).ok())
			.filter_map(|b| serde_json::from_slice(&b).ok())
			.collect();
		out.sort_by_key(|p| p.created);
		Ok(out)
	}

	/// Record a human approval of exactly this pending plan (by digest).
	pub fn approve(&self, pending: &Pending) -> io::Result<()> {
		write_private(&self.approval(&pending.id)?, pending.digest.as_bytes())
	}

	/// Approved, and the stored plan has not been modified since: its digest still matches.
	pub fn is_approved(&self, pending: &Pending) -> bool {
		let Ok(path) = self.approval(&pending.id) else { return false };
		fs::read_to_string(path).is_ok_and(|d| d == pending.digest && pending.plan.digest() == pending.digest)
	}

	pub fn remove(&self, id: &str) -> io::Result<()> {
		let _ = fs::remove_file(self.approval(id)?);
		match fs::remove_file(self.file(id)?) {
			Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
			_ => Ok(()),
		}
	}

	fn file(&self, id: &str) -> io::Result<PathBuf> {
		Ok(self.dir.join(format!("{}.json", check_id(id)?)))
	}

	fn approval(&self, id: &str) -> io::Result<PathBuf> {
		Ok(self.dir.join(format!("{}.approved", check_id(id)?)))
	}
}

/// Ids come from agents: never let one escape the pending directory.
fn check_id(id: &str) -> io::Result<&str> {
	if !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
		Ok(id)
	} else {
		Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			format!("invalid plan id '{}'", tpfs_core::sanitize(id.as_ref())),
		))
	}
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
	#[cfg(unix)]
	{
		use std::os::unix::fs::DirBuilderExt;
		fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
	}
	#[cfg(not(unix))]
	fs::create_dir_all(dir)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
	use std::io::Write;
	let mut options = fs::OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
	let mut f = options.open(path)?;
	f.write_all(bytes)?;
	f.sync_all()
}
