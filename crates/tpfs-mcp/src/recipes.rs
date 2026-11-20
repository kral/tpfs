//! Recipes proposed by agents, waiting for a person to install or reject them in a front end.

use std::{
	fs, io,
	path::{Path, PathBuf},
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tpfs_recipe::Recipe;

use crate::store::{check_id, create_private_dir, write_private};

/// Most recipes waiting for a decision at once.
pub const MAX_PENDING_RECIPES: usize = 5;
/// How long an install or a rejection is remembered for `recipe_status`.
const DECISION_KEPT: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A recipe proposed by an agent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingRecipe {
	pub id: String,
	pub created: u64,
	/// The MCP client's own name for itself (not verified).
	pub client: String,
	pub note: String,
	pub recipe: Recipe,
	/// [`Recipe::hash`] when it was proposed.
	pub hash: String,
}

/// Where a proposal stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeStatus {
	Pending,
	Installed,
	Rejected,
	Unknown,
}

impl RecipeStatus {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Pending => "pending",
			Self::Installed => "installed",
			Self::Rejected => "rejected",
			Self::Unknown => "unknown",
		}
	}
}

/// The recipe inbox (`<state>/pending-recipes`), private like the plan inbox. Installing is the
/// front end's job; this only records that a proposal was taken or refused.
#[derive(Clone, Debug)]
pub struct RecipeStore {
	dir: PathBuf,
}

impl RecipeStore {
	pub fn new(state: &Path) -> Self {
		Self { dir: state.join("pending-recipes") }
	}

	pub fn put(&self, recipe: &Recipe, client: &str, note: &str) -> io::Result<PendingRecipe> {
		create_private_dir(&self.dir)?;
		self.forget_old_decisions();
		if self.waiting()? >= MAX_PENDING_RECIPES {
			return Err(io::Error::other(format!(
				"too many recipes are waiting (limit {MAX_PENDING_RECIPES}); ask the user to install or reject some first"
			)));
		}
		let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
		let pending = PendingRecipe {
			id: format!("{:x}{:x}", nanos, std::process::id() & 0xfff),
			created: (nanos / 1_000_000_000) as u64,
			client: client.chars().take(64).collect(),
			note: note.to_owned(),
			recipe: recipe.clone(),
			hash: recipe.hash(),
		};
		let bytes = serde_json::to_vec_pretty(&pending).map_err(io::Error::other)?;
		write_private(&self.path(&pending.id, "json")?, &bytes)?;
		Ok(pending)
	}

	/// Every waiting recipe, oldest first, and the ids of files that cannot be read.
	pub fn scan(&self) -> io::Result<(Vec<PendingRecipe>, Vec<String>)> {
		let (mut found, mut damaged) = (Vec::new(), Vec::new());
		let entries = match fs::read_dir(&self.dir) {
			Ok(e) => e,
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((found, damaged)),
			Err(e) => return Err(e),
		};
		for e in entries.filter_map(Result::ok) {
			let path = e.path();
			if path.extension().is_none_or(|x| x != "json") {
				continue;
			}
			let Some(id) = path.file_stem().and_then(|s| s.to_str()).filter(|id| check_id(id).is_ok()) else {
				continue;
			};
			match fs::read(&path).ok().and_then(|b| serde_json::from_slice::<PendingRecipe>(&b).ok()) {
				// A stored recipe is checked again: the file is not trusted more than the agent was.
				Some(p) if p.id == id && p.recipe.check().is_ok() && p.recipe.hash() == p.hash => found.push(p),
				_ => damaged.push(id.to_owned()),
			}
		}
		found.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
		damaged.sort();
		Ok((found, damaged))
	}

	pub fn get(&self, id: &str) -> io::Result<PendingRecipe> {
		let bytes = fs::read(self.path(id, "json")?).map_err(|e| match e.kind() {
			io::ErrorKind::NotFound => {
				io::Error::new(e.kind(), format!("no pending recipe '{id}' ({})", self.status(id).as_str()))
			}
			_ => e,
		})?;
		let p: PendingRecipe = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
		if p.id != id || p.recipe.check().is_err() || p.recipe.hash() != p.hash {
			return Err(io::Error::other(format!("pending recipe '{id}' is damaged")));
		}
		Ok(p)
	}

	/// Record that the person installed exactly the recipe they were shown (by hash).
	pub fn mark_installed(&self, id: &str, hash: &str) -> io::Result<()> {
		let p = self.get(id)?;
		if p.hash != hash {
			return Err(io::Error::other("the recipe is not the one that was shown (its hash differs)"));
		}
		self.decide(id, "installed", hash.as_bytes())
	}

	/// Record that the person rejected the recipe; a damaged file can be rejected too.
	pub fn reject(&self, id: &str) -> io::Result<()> {
		if fs::symlink_metadata(self.path(id, "json")?).is_err() {
			return Err(io::Error::new(io::ErrorKind::NotFound, format!("no pending recipe '{id}'")));
		}
		self.decide(id, "rejected", b"")
	}

	pub fn status(&self, id: &str) -> RecipeStatus {
		let is = |ext| self.path(id, ext).is_ok_and(|p| fs::symlink_metadata(p).is_ok_and(|m| m.is_file()));
		if is("json") {
			RecipeStatus::Pending
		} else if is("installed") {
			RecipeStatus::Installed
		} else if is("rejected") {
			RecipeStatus::Rejected
		} else {
			RecipeStatus::Unknown
		}
	}

	fn decide(&self, id: &str, ext: &str, bytes: &[u8]) -> io::Result<()> {
		write_private(&self.path(id, ext)?, bytes)?;
		fs::remove_file(self.path(id, "json")?)
	}

	fn waiting(&self) -> io::Result<usize> {
		Ok(fs::read_dir(&self.dir)?
			.filter_map(Result::ok)
			.filter(|e| e.path().extension().is_some_and(|x| x == "json"))
			.filter(|e| e.path().file_stem().and_then(|s| s.to_str()).is_some_and(|id| check_id(id).is_ok()))
			.count())
	}

	/// Installs and rejections older than a month are forgotten (best effort).
	fn forget_old_decisions(&self) {
		let Ok(entries) = fs::read_dir(&self.dir) else { return };
		for e in entries.filter_map(Result::ok) {
			let path = e.path();
			let decision = path.extension().is_some_and(|x| x == "installed" || x == "rejected");
			let old =
				|| e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|a| a >= DECISION_KEPT));
			if decision && old() {
				fs::remove_file(path).ok();
			}
		}
	}

	fn path(&self, id: &str, ext: &str) -> io::Result<PathBuf> {
		Ok(self.dir.join(format!("{}.{ext}", check_id(id)?)))
	}
}

/// A recipe the person installed, as kept in `<state>/recipes/<recipe id>.json`. The format is
/// shared by every front end on the same state folder.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installed {
	pub recipe: Recipe,
	/// [`Recipe::hash`] of the recipe the person reviewed.
	pub hash: String,
	/// The scope the person confirmed (absolute folders; may be narrower than the recipe's).
	pub roots: Vec<PathBuf>,
	/// Unix seconds.
	pub installed: u64,
	/// Completed runs; a front end may ask for more care during the first few.
	#[serde(default)]
	pub runs: u32,
}

/// Installed recipes (`<state>/recipes`), private like the inboxes.
#[derive(Clone, Debug)]
pub struct InstalledRecipes {
	dir: PathBuf,
}

impl InstalledRecipes {
	pub fn new(state: &Path) -> Self {
		Self { dir: state.join("recipes") }
	}

	/// Install (or update to a newer version) exactly the reviewed recipe within `roots`.
	pub fn install(&self, recipe: &Recipe, hash: &str, roots: &[PathBuf]) -> io::Result<Installed> {
		if recipe.hash() != hash {
			return Err(io::Error::other("the recipe is not the one that was reviewed (its hash differs)"));
		}
		recipe.check().map_err(|e| io::Error::other(e[0].to_string()))?;
		if let Some(old) = self.get(&recipe.id).ok().filter(|old| old.recipe.version >= recipe.version) {
			return Err(io::Error::new(
				io::ErrorKind::AlreadyExists,
				format!(
					"version {} of '{}' is installed; only a newer version replaces it",
					old.recipe.version, recipe.id
				),
			));
		}
		create_private_dir(&self.dir)?;
		let installed = Installed {
			recipe: recipe.clone(),
			hash: hash.to_owned(),
			roots: roots.to_vec(),
			installed: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
			runs: 0,
		};
		self.write(&installed)?;
		Ok(installed)
	}

	/// An installed recipe, refused if its file was changed outside a front end (it must be
	/// reviewed again).
	pub fn get(&self, id: &str) -> io::Result<Installed> {
		let file = self.file(id)?;
		let bytes = fs::read(&file).map_err(|e| match e.kind() {
			io::ErrorKind::NotFound => io::Error::new(e.kind(), format!("no installed recipe '{id}'")),
			_ => e,
		})?;
		let i: Installed = serde_json::from_slice(&bytes)
			.map_err(|e| io::Error::other(format!("installed recipe '{id}' is unreadable: {e}")))?;
		if i.recipe.id != id || i.recipe.check().is_err() || i.recipe.hash() != i.hash {
			return Err(io::Error::other(format!(
				"installed recipe '{id}' was changed outside tpfs; it stays disabled until it is installed again from a proposal"
			)));
		}
		Ok(i)
	}

	/// Every installed recipe by id, and the ids that cannot be used (unreadable or changed).
	pub fn list(&self) -> io::Result<(Vec<Installed>, Vec<String>)> {
		let (mut ok, mut bad) = (Vec::new(), Vec::new());
		let entries = match fs::read_dir(&self.dir) {
			Ok(e) => e,
			Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((ok, bad)),
			Err(e) => return Err(e),
		};
		for e in entries.filter_map(Result::ok) {
			let path = e.path();
			if path.extension().is_none_or(|x| x != "json") {
				continue;
			}
			let Some(id) = path.file_stem().and_then(|s| s.to_str()).filter(|id| recipe_id_ok(id)) else { continue };
			match self.get(id) {
				Ok(i) => ok.push(i),
				Err(_) => bad.push(id.to_owned()),
			}
		}
		ok.sort_by(|a, b| a.recipe.id.cmp(&b.recipe.id));
		bad.sort();
		Ok((ok, bad))
	}

	pub fn remove(&self, id: &str) -> io::Result<()> {
		fs::remove_file(self.file(id)?).map_err(|e| match e.kind() {
			io::ErrorKind::NotFound => io::Error::new(e.kind(), format!("no installed recipe '{id}'")),
			_ => e,
		})
	}

	/// Count a completed run.
	pub fn record_run(&self, id: &str) -> io::Result<()> {
		let mut i = self.get(id)?;
		i.runs = i.runs.saturating_add(1);
		self.write(&i)
	}

	fn write(&self, i: &Installed) -> io::Result<()> {
		let bytes = serde_json::to_vec_pretty(i).map_err(io::Error::other)?;
		// Written beside and renamed: a crash never leaves half a recipe.
		let part = self.dir.join(format!(".{}.part", i.recipe.id));
		write_private(&part, &bytes)?;
		fs::rename(&part, self.file(&i.recipe.id)?)
	}

	fn file(&self, id: &str) -> io::Result<PathBuf> {
		if !recipe_id_ok(id) {
			return Err(io::Error::new(
				io::ErrorKind::InvalidInput,
				format!("invalid recipe id '{}'", tpfs_core::sanitize(id.as_ref())),
			));
		}
		Ok(self.dir.join(format!("{id}.json")))
	}
}

fn recipe_id_ok(id: &str) -> bool {
	(3..=48).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
	use super::*;

	fn recipe() -> Recipe {
		tpfs_recipe::parse(include_str!("../../tpfs-recipe/examples/photos-by-date.json")).unwrap()
	}

	#[test]
	fn a_sixth_pending_recipe_is_refused() {
		let tmp = tempfile::tempdir().unwrap();
		let store = RecipeStore::new(tmp.path());
		let ids: Vec<_> = (0..MAX_PENDING_RECIPES).map(|_| store.put(&recipe(), "test", "").unwrap().id).collect();
		assert!(store.put(&recipe(), "test", "").is_err());
		store.reject(&ids[0]).unwrap();
		store.put(&recipe(), "test", "").unwrap();
		assert_eq!(store.scan().unwrap().0.len(), MAX_PENDING_RECIPES);
	}

	#[test]
	fn status_follows_the_decision() {
		let tmp = tempfile::tempdir().unwrap();
		let store = RecipeStore::new(tmp.path());
		let a = store.put(&recipe(), "claude-code", "renames photos").unwrap();
		let b = store.put(&recipe(), "claude-code", "").unwrap();
		assert_eq!(store.status(&a.id), RecipeStatus::Pending);
		assert_eq!(store.get(&a.id).unwrap(), a);
		store.reject(&a.id).unwrap();
		assert_eq!(store.status(&a.id), RecipeStatus::Rejected);
		assert!(store.mark_installed(&b.id, "not-the-hash").is_err());
		assert_eq!(store.status(&b.id), RecipeStatus::Pending);
		store.mark_installed(&b.id, &b.hash).unwrap();
		assert_eq!(store.status(&b.id), RecipeStatus::Installed);
		assert_eq!(store.status("abc"), RecipeStatus::Unknown);
		assert_eq!(store.status("../x"), RecipeStatus::Unknown);
		assert!(store.get(&b.id).unwrap_err().to_string().contains("installed"));
		assert!(store.scan().unwrap().0.is_empty());
	}

	#[test]
	fn a_tampered_recipe_is_listed_as_damaged() {
		let tmp = tempfile::tempdir().unwrap();
		let store = RecipeStore::new(tmp.path());
		let p = store.put(&recipe(), "x", "").unwrap();
		let file = tmp.path().join("pending-recipes").join(format!("{}.json", p.id));
		let text = fs::read_to_string(&file).unwrap().replace("Rename photos by date", "Something else");
		fs::write(&file, text).unwrap();
		assert_eq!(store.scan().unwrap(), (vec![], vec![p.id.clone()]));
		assert!(store.get(&p.id).is_err());
		store.reject(&p.id).unwrap();
		assert_eq!(store.status(&p.id), RecipeStatus::Rejected);
	}

	#[test]
	fn installed_recipes_are_kept_by_hash_and_version() {
		let tmp = tempfile::tempdir().unwrap();
		let shelf = InstalledRecipes::new(tmp.path());
		let r = recipe();
		assert!(shelf.install(&r, "bad-hash", &[]).is_err());
		let roots = vec![PathBuf::from("/data")];
		shelf.install(&r, &r.hash(), &roots).unwrap();
		assert_eq!(shelf.get(&r.id).unwrap().roots, roots);
		assert_eq!(shelf.install(&r, &r.hash(), &roots).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
		let mut newer = r.clone();
		newer.version += 1;
		shelf.install(&newer, &newer.hash(), &roots).unwrap();
		shelf.record_run(&r.id).unwrap();
		let (list, bad) = shelf.list().unwrap();
		assert_eq!((list.len(), list[0].runs, list[0].recipe.version, bad.len()), (1, 1, newer.version, 0));
		// Changed outside tpfs: disabled.
		let file = tmp.path().join("recipes").join(format!("{}.json", r.id));
		let text = fs::read_to_string(&file).unwrap().replace("Rename photos by date", "Rename photos");
		fs::write(&file, text).unwrap();
		assert!(shelf.get(&r.id).unwrap_err().to_string().contains("changed outside"));
		assert_eq!(shelf.list().unwrap().1, vec![r.id.clone()]);
		shelf.remove(&r.id).unwrap();
		assert!(shelf.get(&r.id).is_err());
		assert!(shelf.get("../x").is_err());
	}
}
