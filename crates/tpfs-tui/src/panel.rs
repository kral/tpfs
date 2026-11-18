use std::{
	collections::HashSet,
	ffi::OsString,
	fs,
	path::{Path, PathBuf},
};

use tpfs_core::sanitize;

#[derive(Clone, Debug)]
pub(crate) struct Entry {
	pub name: OsString,
	/// Sanitised for display: control characters, bidi overrides and invalid UTF-8 are escaped.
	pub display: String,
	pub is_dir: bool,
	pub size: u64,
	pub is_up: bool,
	pub modified: Option<std::time::SystemTime>,
}

/// One panel: a directory listing with a cursor and a set of marked entries.
pub(crate) struct Panel {
	/// Always canonical (no symlinks), so plans built from it pass tpfs validation.
	pub cwd: PathBuf,
	pub entries: Vec<Entry>,
	pub cursor: usize,
	pub offset: usize,
	pub marked: HashSet<OsString>,
	pub error: Option<String>,
}

impl Panel {
	pub fn new(path: &Path) -> Self {
		let cwd = tpfs_core::canonicalize(path).unwrap_or_else(|_| PathBuf::from("/"));
		let mut p = Self { cwd, entries: vec![], cursor: 0, offset: 0, marked: HashSet::new(), error: None };
		p.reload();
		p
	}

	/// Re-read the directory, keeping the cursor on the same name if it still exists.
	pub fn reload(&mut self) {
		let keep = self.current().map(|e| e.name.clone());
		self.entries.clear();
		self.error = None;

		if self.cwd.parent().is_some() {
			self.entries.push(Entry {
				name: "..".into(),
				display: "..".into(),
				is_dir: true,
				size: 0,
				is_up: true,
				modified: None,
			});
		}

		match fs::read_dir(&self.cwd) {
			Ok(rd) => {
				let mut list: Vec<Entry> = rd
					.filter_map(Result::ok)
					.map(|e| {
						let name = e.file_name();
						let meta = fs::metadata(e.path()).or_else(|_| fs::symlink_metadata(e.path()));
						let is_dir = meta.as_ref().is_ok_and(|m| m.is_dir());
						let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
						let modified = meta.ok().and_then(|m| m.modified().ok());
						Entry { display: sanitize(&name), name, is_dir, size, is_up: false, modified }
					})
					.collect();
				list.sort_by(|a, b| {
					b.is_dir.cmp(&a.is_dir).then_with(|| a.display.to_lowercase().cmp(&b.display.to_lowercase()))
				});
				self.entries.extend(list);
			}
			Err(e) => self.error = Some(e.to_string()),
		}

		self.marked.retain(|m| self.entries.iter().any(|e| &e.name == m));
		self.cursor = keep
			.and_then(|k| self.entries.iter().position(|e| e.name == k))
			.unwrap_or(0)
			.min(self.entries.len().saturating_sub(1));
	}

	pub fn current(&self) -> Option<&Entry> {
		self.entries.get(self.cursor)
	}

	pub fn set_cwd(&mut self, path: &Path, focus: Option<OsString>) {
		match tpfs_core::canonicalize(path) {
			Ok(p) if p.is_dir() => {
				self.cwd = p;
				self.marked.clear();
				self.offset = 0;
				self.cursor = 0;
				self.reload();
				if let Some(f) = focus
					&& let Some(i) = self.entries.iter().position(|e| e.name == f)
				{
					self.cursor = i;
				}
			}
			Ok(_) => {}
			Err(e) => self.error = Some(e.to_string()),
		}
	}

	/// Enter the directory under the cursor. Returns false when it is a file.
	pub fn enter(&mut self) -> bool {
		let Some(e) = self.current().cloned() else { return false };
		if e.is_up {
			self.up();
			true
		} else if e.is_dir {
			let target = self.cwd.join(&e.name);
			self.set_cwd(&target, None);
			true
		} else {
			false
		}
	}

	pub fn up(&mut self) {
		let Some(parent) = self.cwd.parent().map(Path::to_path_buf) else { return };
		let from = self.cwd.file_name().map(|n| n.to_os_string());
		self.set_cwd(&parent, from);
	}

	pub fn move_cursor(&mut self, delta: isize) {
		if self.entries.is_empty() {
			return;
		}
		let last = self.entries.len() as isize - 1;
		self.cursor = (self.cursor as isize + delta).clamp(0, last) as usize;
	}

	pub fn toggle_mark(&mut self) {
		let Some(e) = self.current().cloned() else { return };
		if !e.is_up && !self.marked.remove(&e.name) {
			self.marked.insert(e.name);
		}
		self.move_cursor(1);
	}

	/// Marked entries, or the one under the cursor when nothing is marked (Norton Commander semantics).
	pub fn selection(&self) -> Vec<PathBuf> {
		if !self.marked.is_empty() {
			return self
				.entries
				.iter()
				.filter(|e| self.marked.contains(&e.name))
				.map(|e| self.cwd.join(&e.name))
				.collect();
		}
		self.current().filter(|e| !e.is_up).map(|e| vec![self.cwd.join(&e.name)]).unwrap_or_default()
	}

	pub fn marked_size(&self) -> u64 {
		self.entries.iter().filter(|e| self.marked.contains(&e.name) && !e.is_dir).map(|e| e.size).sum()
	}

	/// Keep the cursor inside a viewport of `height` rows.
	pub fn scroll_into_view(&mut self, height: usize) {
		if height == 0 {
			return;
		}
		if self.cursor < self.offset {
			self.offset = self.cursor;
		} else if self.cursor >= self.offset + height {
			self.offset = self.cursor + 1 - height;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn lists_dirs_first_and_tracks_selection() {
		let tmp = tempfile::TempDir::new().unwrap();
		fs::write(tmp.path().join("b.txt"), "bb").unwrap();
		fs::write(tmp.path().join("a.txt"), "a").unwrap();
		fs::create_dir(tmp.path().join("zdir")).unwrap();

		let mut p = Panel::new(tmp.path());
		let names: Vec<_> = p.entries.iter().map(|e| e.display.as_str()).collect();
		assert_eq!(names, ["..", "zdir", "a.txt", "b.txt"]);

		p.cursor = 0;
		assert!(p.selection().is_empty(), "'..' is never selected");
		p.cursor = 2;
		p.toggle_mark();
		p.toggle_mark();
		assert_eq!(p.selection().len(), 2);
		assert_eq!(p.marked_size(), 3);

		p.cursor = 1;
		assert!(p.enter());
		assert!(p.cwd.ends_with("zdir"));
		p.up();
		assert_eq!(p.current().unwrap().display, "zdir", "cursor returns to the folder we came from");
	}
}
