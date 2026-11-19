use std::{
	fs, io,
	path::{Path, PathBuf},
	time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tpfs_core::Plan;

/// Most plans waiting for a decision at once.
pub const MAX_PENDING: usize = 100;

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
		// Only what `tpfs pending` shows (and can reject) counts against the limit.
		let waiting = fs::read_dir(&self.dir)?
			.filter_map(Result::ok)
			.filter(|e| e.path().extension().is_some_and(|x| x == "json"))
			.filter(|e| e.path().file_stem().and_then(|s| s.to_str()).is_some_and(|id| check_id(id).is_ok()))
			.count();
		if waiting >= MAX_PENDING {
			return Err(io::Error::other(format!(
				"too many plans are waiting (limit {MAX_PENDING}); ask the user to approve or reject some (`tpfs pending`)"
			)));
		}
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
		let file = self.file(id)?;
		let bytes = fs::read(&file).map_err(|e| match e.kind() {
			io::ErrorKind::NotFound => io::Error::new(
				e.kind(),
				format!("no pending plan '{id}' (already applied, rejected or never proposed)"),
			),
			_ => io::Error::new(e.kind(), format!("{}: {e}", tpfs_core::sanitize(file.as_os_str()))),
		})?;
		serde_json::from_slice(&bytes).map_err(|e| {
			io::Error::other(format!(
				"pending plan '{id}' is unreadable ({}); propose it again, or ask the user to reject it (`tpfs pending` lists it)",
				tpfs_core::sanitize(e.to_string().as_ref())
			))
		})
	}

	/// Every pending plan, oldest first.
	pub fn list(&self) -> io::Result<Vec<Pending>> {
		Ok(self.scan()?.0)
	}

	/// Every pending plan, oldest first, and the ids of the ones that cannot be read or parsed
	/// (listed so they can be rejected), in one read. No pending folder means none waiting.
	pub fn scan(&self) -> io::Result<(Vec<Pending>, Vec<String>)> {
		let (mut plans, mut damaged) = (Vec::new(), Vec::new());
		let entries = match fs::read_dir(&self.dir) {
			Ok(entries) => entries,
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((plans, damaged)),
			Err(e) => {
				return Err(io::Error::new(
					e.kind(),
					format!("cannot read {}: {e}", tpfs_core::sanitize(self.dir.as_os_str())),
				));
			}
		};
		for e in entries.filter_map(Result::ok) {
			let path = e.path();
			if path.extension().is_none_or(|x| x != "json") {
				continue;
			}
			let Some(id) = path.file_stem().and_then(|s| s.to_str()).filter(|id| check_id(id).is_ok()) else {
				continue; // not a name `tpfs pending` could show or `tpfs reject` take
			};
			// Listed under its file name: a plan whose own id differs is damaged (`tpfs reject` takes
			// the file name).
			match fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Pending>(&b).ok()) {
				Some(p) if p.id == id => plans.push(p),
				_ => damaged.push(id.to_owned()),
			}
		}
		// Ids grow with time: they order plans made within the same second.
		plans.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
		damaged.sort();
		Ok((plans, damaged))
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
		match fs::remove_file(self.approval(id)?) {
			Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
			_ => {}
		}
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
	tpfs_core::private_dir(dir)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
	use std::io::Write;
	let mut options = fs::OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;
		// O_NOFOLLOW: a symlink planted after the check below is refused, not followed.
		options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
	}
	// Never write through a planted symlink (e.g. an `.approved` file pointing at ~/.bashrc).
	tpfs_core::check_private_file(path)?;
	let mut f = options.open(path)?;
	f.write_all(bytes)?;
	f.sync_all()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn plan() -> Plan {
		Plan::new(vec![tpfs_core::Op::Mkdir { path: "/x/y".into() }], "")
	}

	/// One read: valid plans oldest first; a damaged file, and a plan whose own id is not its file
	/// name, listed as damaged (so `tpfs reject` can take them); other names ignored.
	#[test]
	fn scan_lists_plans_and_damaged_files_by_name() {
		let tmp = tempfile::TempDir::new().unwrap();
		let store = Store::new(tmp.path());
		let a = store.put(&plan(), &[]).unwrap();
		std::thread::sleep(std::time::Duration::from_millis(2));
		let b = store.put(&plan(), &[]).unwrap();
		let dir = tmp.path().join("pending");
		fs::write(dir.join("0123abcd.json"), "not json").unwrap();
		let mut moved = a.clone();
		moved.id = "fedcba98".into();
		fs::write(dir.join("deadbeef.json"), serde_json::to_vec(&moved).unwrap()).unwrap();
		fs::write(dir.join("zz.json"), "{}").unwrap();
		fs::write(dir.join(format!("{}.approved", a.id)), "x").unwrap();
		let (plans, damaged) = store.scan().unwrap();
		assert_eq!(plans.iter().map(|p| p.id.as_str()).collect::<Vec<_>>().len(), 2);
		assert!(plans.iter().any(|p| p.id == a.id) && plans.iter().any(|p| p.id == b.id));
		assert!(plans.windows(2).all(|w| w[0].created <= w[1].created));
		assert_eq!(damaged, ["0123abcd", "deadbeef"]);
	}

	/// Files `tpfs pending` cannot show do not use up the limit of waiting plans.
	#[test]
	fn only_listed_plans_count_toward_the_limit() {
		let tmp = tempfile::TempDir::new().unwrap();
		let store = Store::new(tmp.path());
		store.put(&plan(), &[]).unwrap();
		let dir = tmp.path().join("pending");
		for i in 0..MAX_PENDING {
			fs::write(dir.join(format!("junk-{i}.json")), "{}").unwrap();
		}
		store.put(&plan(), &[]).unwrap();
	}

	/// An unreadable pending folder is an error naming it, not "no plans".
	#[cfg(unix)]
	#[test]
	fn an_unreadable_pending_folder_is_reported() {
		use std::os::unix::fs::PermissionsExt;
		let tmp = tempfile::TempDir::new().unwrap();
		let store = Store::new(tmp.path());
		store.put(&plan(), &[]).unwrap();
		let dir = tmp.path().join("pending");
		fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
		if fs::read_dir(&dir).is_ok() {
			fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
			eprintln!("skipped: running as root, which reads any folder");
			return;
		}
		let err = store.scan().unwrap_err();
		fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
		assert!(err.to_string().contains("pending"), "{err}");
	}
}
