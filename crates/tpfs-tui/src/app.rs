use std::{
	io,
	path::PathBuf,
	time::{Duration, Instant},
};

use ratatui::{
	crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
	layout::Rect,
};
use tpfs_core::{
	BatchStatus, ExecError, Executor, Issue, Journal, Op, Plan, Source, Validator,
	risk::{self, Risk},
};

use crate::panel::Panel;

pub(crate) enum Dialog {
	/// A plan waiting for confirmation, with its risk and any validation problems.
	Confirm {
		plan: Plan,
		risk: Risk,
		issues: Vec<Issue>,
		/// First visible line of the operation list.
		offset: usize,
		/// Set by drawing: the whole dialog (risk, keys) fits the terminal…
		fits: bool,
		/// …and every operation line has been on screen at least once.
		seen_all: bool,
		/// Totals with sizes and the short digest, worked out once when the dialog opens.
		summary: String,
	},
	Input {
		title: &'static str,
		value: String,
		kind: InputKind,
	},
	Help,
	/// F9 / Ctrl+Z: which batch would be undone (it may come from an agent or another terminal).
	Undo {
		batch: String,
		lines: Vec<String>,
	},
	/// Shift+F2: compare the two panels' folders and preview a sync before building the plan.
	Sync {
		cmp: tpfs_core::sync::Comparison,
		opts: tpfs_core::sync::SyncOptions,
		offset: usize,
	},
	/// Alt+F1 / Alt+F2: pick a drive (Windows) or mount point for one panel.
	Drives {
		panel: usize,
		items: Vec<PathBuf>,
		cursor: usize,
	},
	/// F3: read-only view of a file (sanitised text or a hex dump).
	View {
		title: String,
		lines: Vec<String>,
		offset: usize,
	},
}

/// Something that needs the real terminal: the TUI is suspended while it runs.
pub(crate) enum External {
	Edit(PathBuf),
	Shell { command: String, cwd: PathBuf },
}

pub(crate) enum InputKind {
	Mkdir,
	Rename(PathBuf),
}

/// F-key bar: (label, key). Empty labels are not bound in the minimal commander.
pub(crate) const FKEYS: [&str; 10] =
	["Help", "Rename", "View", "Edit", "Copy", "Move", "Mkdir", "Trash", "Undo", "Quit"];

pub struct App {
	pub(crate) panels: [Panel; 2],
	pub(crate) active: usize,
	pub(crate) dialog: Option<Dialog>,
	pub(crate) status: String,
	pub(crate) quit: bool,
	/// The command line, while it is being edited (`:` opens it).
	pub(crate) command: Option<String>,
	/// Quick search: letters typed so far, when the last one came, and whether anything matched.
	pub(crate) quick: Option<Quick>,
	pub(crate) external: Option<External>,
	pub(crate) icons: bool,
	state: PathBuf,
	validator: Validator,
	/// Screen areas from the last draw, for mouse hit-testing.
	pub(crate) areas: Areas,
	last_click: Option<(Instant, usize, usize)>,
}

pub(crate) struct Quick {
	pub text: String,
	pub at: Instant,
	pub miss: bool,
}

/// Quick search starts over after this long without typing.
const QUICK_RESET: Duration = Duration::from_millis(1200);

#[derive(Default, Clone, Copy)]
pub(crate) struct Areas {
	pub panels: [Rect; 2],
	pub fkeys: Rect,
}

impl App {
	pub fn new(left: PathBuf, right: PathBuf, state: PathBuf, icons: bool) -> io::Result<Self> {
		// The human may work anywhere; confinement against symlinks still applies below the root.
		let validator = Validator::new(tpfs_core::system_roots())?.with_protected([&state]);
		Ok(Self {
			panels: [Panel::new(&left), Panel::new(&right)],
			active: 0,
			dialog: None,
			status: "F1 help · Tab switch panel · Ins/Space mark · Ctrl+Z undo".into(),
			quit: false,
			command: None,
			quick: None,
			external: None,
			icons,
			state,
			validator,
			areas: Areas::default(),
			last_click: None,
		})
	}

	fn panel(&mut self) -> &mut Panel {
		&mut self.panels[self.active]
	}

	fn other(&self) -> &Panel {
		&self.panels[1 - self.active]
	}

	pub fn on_key(&mut self, key: KeyEvent) {
		if key.kind == KeyEventKind::Release {
			return;
		}
		if self.dialog.is_some() {
			self.on_dialog_key(key);
			return;
		}
		if self.command.is_some() {
			self.on_command_key(key);
			return;
		}

		let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
		let alt = key.modifiers.contains(KeyModifiers::ALT);

		// Quick search: letters jump to the first name that starts with what was typed.
		let recent = self.quick.as_ref().filter(|q| q.at.elapsed() < QUICK_RESET).map(|q| q.text.clone());
		match key.code {
			KeyCode::Char(c) if !ctrl && !alt && c != ':' && c != ' ' => {
				let text = recent.unwrap_or_default() + &c.to_string();
				return self.quick_search(text);
			}
			KeyCode::Backspace if recent.is_some() => {
				let mut text = recent.unwrap_or_default();
				text.pop();
				return if text.is_empty() { self.quick = None } else { self.quick_search(text) };
			}
			KeyCode::Esc if self.quick.is_some() => {
				self.quick = None;
				return;
			}
			_ => self.quick = None,
		}

		match key.code {
			KeyCode::F(n @ (1 | 2)) if alt => self.open_drives(n as usize - 1),
			KeyCode::F(2) if key.modifiers.contains(KeyModifiers::SHIFT) => self.open_sync(),
			// Some terminals report Shift+F2 as F14.
			KeyCode::F(14) => self.open_sync(),
			KeyCode::Char('c' | 'q') if ctrl => self.quit = true,
			KeyCode::Char('z') if ctrl => self.undo(),
			KeyCode::Char('r') if ctrl => self.reload(),
			KeyCode::Tab | KeyCode::BackTab => self.active = 1 - self.active,
			KeyCode::Up => self.panel().move_cursor(-1),
			KeyCode::Down => self.panel().move_cursor(1),
			KeyCode::PageUp => self.panel().move_cursor(-20),
			KeyCode::PageDown => self.panel().move_cursor(20),
			KeyCode::Home => self.panel().move_cursor(isize::MIN / 2),
			KeyCode::End => self.panel().move_cursor(isize::MAX / 2),
			KeyCode::Enter => {
				if !self.panel().enter() {
					self.fkey(3);
				}
			}
			KeyCode::Char(':') => self.command = Some(String::new()),
			KeyCode::Backspace | KeyCode::Left => self.up(),
			KeyCode::Right => {
				self.panel().enter();
			}
			KeyCode::Insert | KeyCode::Char(' ') => self.panel().toggle_mark(),
			KeyCode::Delete => self.fkey(8),
			KeyCode::F(n) if (1..=10).contains(&n) => self.fkey(n),
			_ => {}
		}
	}

	/// Function keys, also reachable by clicking the F-key bar.
	pub(crate) fn fkey(&mut self, n: u8) {
		match n {
			1 => self.dialog = Some(Dialog::Help),
			3 => {
				let Some(e) = self.panel().current().filter(|e| !e.is_dir).cloned() else { return };
				let path = self.panels[self.active].cwd.join(&e.name);
				match crate::extra::view_lines(&path) {
					Ok(lines) => self.dialog = Some(Dialog::View { title: e.display.clone(), lines, offset: 0 }),
					Err(err) => self.status = format!("Cannot view {}: {err}", e.display),
				}
			}
			4 => {
				let Some(e) = self.panel().current().filter(|e| !e.is_dir).cloned() else { return };
				self.external = Some(External::Edit(self.panels[self.active].cwd.join(&e.name)));
			}
			2 => {
				let Some(e) = self.panel().current().filter(|e| !e.is_up).cloned() else { return };
				let path = self.panels[self.active].cwd.join(&e.name);
				self.dialog = Some(Dialog::Input {
					title: "Rename",
					value: e.name.to_string_lossy().into_owned(),
					kind: InputKind::Rename(path),
				});
			}
			5 | 6 => {
				let dest = self.other().cwd.clone();
				let ops: Vec<Op> = self.panels[self.active]
					.selection()
					.into_iter()
					.filter_map(|from| {
						let to = dest.join(from.file_name()?);
						Some(if n == 5 { Op::Copy { from, to } } else { Op::Move { from, to } })
					})
					.collect();
				let verb = if n == 5 { "Copy" } else { "Move" };
				self.propose(Plan::new(ops, format!("{verb} to {}", tpfs_core::sanitize(dest.as_os_str()))));
			}
			7 => {
				self.dialog =
					Some(Dialog::Input { title: "Make directory", value: String::new(), kind: InputKind::Mkdir })
			}
			8 => {
				let ops: Vec<Op> =
					self.panels[self.active].selection().into_iter().map(|path| Op::Trash { path }).collect();
				self.propose(Plan::new(ops, "Move to trash"));
			}
			9 => self.undo(),
			10 => self.quit = true,
			_ => {}
		}
	}

	fn quick_search(&mut self, text: String) {
		let needle = fold(&text);
		let panel = &mut self.panels[self.active];
		let hit = panel.entries.iter().position(|e| !e.is_up && fold(&e.display).starts_with(&needle));
		if let Some(i) = hit {
			panel.cursor = i;
		}
		self.quick = Some(Quick { text, at: Instant::now(), miss: hit.is_none() });
	}

	/// Compare left and right; the default direction copies from the active panel to the other.
	pub(crate) fn open_sync(&mut self) {
		use tpfs_core::sync::{Direction, SyncOptions, compare};
		let (l, r) = (self.panels[0].cwd.clone(), self.panels[1].cwd.clone());
		if l.starts_with(&r) || r.starts_with(&l) {
			self.status = "Sync needs two separate folders in the panels (one is inside the other).".into();
			return;
		}
		match compare(&l, &r, true) {
			Ok(cmp) if cmp.truncated => {
				self.status = format!("Too many files to compare (limit {}).", tpfs_core::sync::MAX_ENTRIES)
			}
			Ok(cmp) => {
				let direction = if self.active == 0 { Direction::LeftToRight } else { Direction::RightToLeft };
				self.dialog = Some(Dialog::Sync { cmp, opts: SyncOptions { direction, mirror: false }, offset: 0 });
			}
			Err(e) => self.status = format!("Compare failed: {e}"),
		}
	}

	/// Parent directory; at the top of a drive or mount, offer the drive list instead.
	fn up(&mut self) {
		if self.panels[self.active].cwd.parent().is_none() {
			self.open_drives(self.active);
		} else {
			self.panel().up();
		}
	}

	pub(crate) fn open_drives(&mut self, panel: usize) {
		let items = crate::extra::drives();
		let cwd = &self.panels[panel].cwd;
		// Preselect the deepest entry that contains the panel's folder.
		let cursor = items
			.iter()
			.enumerate()
			.filter(|(_, d)| cwd.starts_with(d))
			.max_by_key(|(_, d)| d.components().count())
			.map_or(0, |(i, _)| i);
		self.dialog = Some(Dialog::Drives { panel, items, cursor });
	}

	fn propose(&mut self, plan: Plan) {
		if plan.ops.is_empty() {
			self.status = "Nothing selected.".into();
			return;
		}
		let issues = self.validator().validate(&plan);
		let risk = risk::assess(&plan, Source::Human);
		let summary = format!(
			"Total: {}\nDigest: {}",
			tpfs_core::risk::totals_line(&plan, " · "),
			plan.digest().get(..12).unwrap_or_default()
		);
		self.dialog = Some(Dialog::Confirm { plan, risk, issues, offset: 0, fits: false, seen_all: false, summary });
	}

	fn on_dialog_key(&mut self, key: KeyEvent) {
		match (self.dialog.take(), key.code) {
			(Some(_), KeyCode::Esc) => {}
			(Some(Dialog::View { title, lines, offset }), code) => {
				let last = lines.len().saturating_sub(1);
				let offset = match code {
					KeyCode::F(3 | 10) | KeyCode::Char('q') => return,
					KeyCode::Up => offset.saturating_sub(1),
					KeyCode::Down => (offset + 1).min(last),
					KeyCode::PageUp => offset.saturating_sub(20),
					KeyCode::PageDown | KeyCode::Char(' ') => (offset + 20).min(last),
					KeyCode::Home => 0,
					KeyCode::End => last,
					_ => offset,
				};
				self.dialog = Some(Dialog::View { title, lines, offset });
			}
			(Some(Dialog::Help), _) => {}
			(Some(Dialog::Undo { batch, .. }), KeyCode::Char('y' | 'Y')) => self.do_undo(&batch),
			(Some(d @ Dialog::Undo { .. }), _) => self.dialog = Some(d),
			(Some(Dialog::Sync { cmp, mut opts, offset }), code) => {
				use tpfs_core::sync::{Direction::*, sync_plan};
				let last = cmp.items.len().saturating_sub(1);
				let offset = match code {
					KeyCode::Enter => {
						let sp = sync_plan(&self.panels[0].cwd, &self.panels[1].cwd, &cmp, opts);
						if !sp.skipped.is_empty() {
							self.status = format!(
								"{} item(s) left alone (conflicts or against the direction).",
								sp.skipped.len()
							);
						}
						if sp.plan.ops.is_empty() {
							self.status = if sp.skipped.is_empty() {
								"Nothing to sync.".into()
							} else {
								format!(
									"Nothing to sync; {} item(s) left alone (conflicts or against the direction).",
									sp.skipped.len()
								)
							};
						} else {
							self.propose(sp.plan);
						}
						return;
					}
					KeyCode::Left => {
						opts.direction = RightToLeft;
						offset
					}
					KeyCode::Right => {
						opts.direction = LeftToRight;
						offset
					}
					KeyCode::Char('b' | 'B') => {
						opts.direction = Both;
						offset
					}
					KeyCode::Char('m' | 'M') if opts.direction != Both => {
						opts.mirror = !opts.mirror;
						offset
					}
					KeyCode::Up => offset.saturating_sub(1),
					KeyCode::Down => (offset + 1).min(last),
					KeyCode::PageUp => offset.saturating_sub(10),
					KeyCode::PageDown => (offset + 10).min(last),
					_ => offset,
				};
				self.dialog = Some(Dialog::Sync { cmp, opts, offset });
			}
			(Some(Dialog::Drives { panel, items, cursor }), code) => {
				let last = items.len().saturating_sub(1);
				let cursor = match code {
					KeyCode::Enter => {
						if let Some(d) = items.get(cursor) {
							self.panels[panel].set_cwd(d, None);
							self.active = panel;
						}
						return;
					}
					KeyCode::Up => cursor.saturating_sub(1),
					KeyCode::Down => (cursor + 1).min(last),
					KeyCode::Home => 0,
					KeyCode::End => last,
					// A drive letter jumps straight to that drive.
					KeyCode::Char(c) => {
						match items.iter().position(|d| drive_letter(d) == Some(c.to_ascii_uppercase())) {
							Some(i) => {
								self.panels[panel].set_cwd(&items[i], None);
								self.active = panel;
								return;
							}
							None => cursor,
						}
					}
					_ => cursor,
				};
				self.dialog = Some(Dialog::Drives { panel, items, cursor });
			}
			// Only a Low-risk plan that removes nothing (a routine F5/F6/F7) proceeds on Enter, so a
			// stray Enter never trashes anything. Otherwise a deliberate `y`; High needs `y` after
			// every operation has been on screen. Nothing proceeds if the dialog does not fit.
			(Some(Dialog::Confirm { plan, risk, issues, mut offset, fits, seen_all, summary }), code) => {
				let ok = issues.is_empty() && fits;
				let level = risk.level;
				match code {
					KeyCode::Enter if ok && level == risk::Level::Low && !removes(&plan) => return self.execute(&plan),
					KeyCode::Char('y' | 'Y') if ok && (level != risk::Level::High || seen_all) => {
						return self.execute(&plan);
					}
					KeyCode::Down => offset += 1,
					KeyCode::Up => offset = offset.saturating_sub(1),
					KeyCode::PageDown | KeyCode::Char(' ') => offset += 10,
					KeyCode::PageUp => offset = offset.saturating_sub(10),
					KeyCode::Home => offset = 0,
					KeyCode::End => offset = usize::MAX / 2,
					_ => {}
				}
				self.dialog = Some(Dialog::Confirm { plan, risk, issues, offset, fits, seen_all, summary });
			}
			(Some(Dialog::Input { title, mut value, kind }), code) => match code {
				KeyCode::Enter => self.submit_input(&value, &kind),
				KeyCode::Backspace => {
					value.pop();
					self.dialog = Some(Dialog::Input { title, value, kind });
				}
				KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
					value.push(c);
					self.dialog = Some(Dialog::Input { title, value, kind });
				}
				_ => self.dialog = Some(Dialog::Input { title, value, kind }),
			},
			(d, _) => self.dialog = d,
		}
	}

	fn on_command_key(&mut self, key: KeyEvent) {
		let Some(mut line) = self.command.take() else { return };
		match key.code {
			KeyCode::Esc => {}
			KeyCode::Enter => {
				if !line.trim().is_empty() {
					let cwd = self.panels[self.active].cwd.clone();
					self.external = Some(External::Shell { command: line, cwd });
				}
			}
			KeyCode::Backspace => {
				line.pop();
				self.command = Some(line);
			}
			KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
				line.push(c);
				self.command = Some(line);
			}
			_ => self.command = Some(line),
		}
	}

	/// Called after an external program returned: the files may have changed under us.
	pub(crate) fn after_external(&mut self, outcome: Result<String, String>) {
		self.status = match outcome {
			Ok(s) => s,
			Err(e) => format!("Failed: {e}"),
		};
		self.reload();
	}

	fn submit_input(&mut self, value: &str, kind: &InputKind) {
		let value = value.trim();
		if value.is_empty() {
			return;
		}
		let plan = match kind {
			// A folder name, not a path: `../x` or `/abs/x` must not create a folder elsewhere.
			InputKind::Mkdir if value == "." || value == ".." || value.contains(['/', '\\', '\0']) => {
				self.status = "A folder name cannot contain / or \\, and cannot be . or ..".into();
				return;
			}
			InputKind::Mkdir => Plan::new(
				vec![Op::Mkdir { path: self.panels[self.active].cwd.join(value) }],
				format!("Make directory {value}"),
			),
			InputKind::Rename(path) => {
				Plan::new(vec![Op::Rename { path: path.clone(), new_name: value.into() }], format!("Rename to {value}"))
			}
		};
		// Single, low-risk operations typed by the user run directly; anything else is previewed.
		let issues = self.validator().validate(&plan);
		if issues.is_empty() && risk::assess(&plan, Source::Human).level == risk::Level::Low {
			self.execute(&plan);
		} else {
			self.propose(plan);
		}
	}

	fn execute(&mut self, plan: &Plan) {
		let result = self.journal().and_then(|mut j| {
			Executor::new(&self.validator(), &mut j, self.state.join("trash"))
				.execute(plan, Source::Human)
				.map(|b| format!("Done: {} operation(s). Ctrl+Z to undo.", b.steps))
				.map_err(|e| one_line(&e))
		});
		self.status = result.unwrap_or_else(|e| format!("Failed: {e}"));
		self.panels.iter_mut().for_each(|p| p.marked.clear());
		self.reload();
	}

	/// Show what Undo would revert and ask first: the newest batch may not be this session's.
	fn undo(&mut self) {
		let batch = self.journal().and_then(|mut j| j.batches().map_err(|e| e.to_string())).map(|b| {
			b.into_iter()
				.rev()
				.find(|b| !matches!(b.status, BatchStatus::Aborted | BatchStatus::Undone | BatchStatus::Abandoned))
		});
		match batch {
			Err(e) => self.status = format!("Undo failed: {e}"),
			Ok(None) => self.status = "Nothing to undo.".into(),
			Ok(Some(b)) if b.status != BatchStatus::Committed || b.damaged => {
				let id = tpfs_core::sanitize(b.id.as_ref());
				self.status = match (b.status, b.damaged) {
					(BatchStatus::Committed, _) => {
						format!("The last batch ({id}) has damaged journal records: undo it by hand from `tpfs log`.")
					}
					(_, true) => format!(
						"The last batch ({id}) was interrupted and its records are damaged: see `tpfs log`, then `tpfs recover --abandon {id}`."
					),
					_ => format!("The last batch ({id}) was interrupted: run `tpfs recover` in a terminal first."),
				};
			}
			Ok(Some(b)) => {
				let who = match b.source.as_str() {
					"agent" => "an AI agent",
					_ => "a person",
				};
				let mut lines = vec![
					format!("Batch {} by {who}", tpfs_core::sanitize(b.id.as_ref())),
					tpfs_core::sanitize(b.reason.as_ref()),
					String::new(),
				];
				let steps: Vec<_> = b.steps.iter().filter(|s| s.done && !s.undone).collect();
				lines.extend(steps.iter().take(12).map(|s| s.forward.normalized().to_string()));
				if steps.len() > 12 {
					lines.push(format!("… and {} more", steps.len() - 12));
				}
				self.dialog = Some(Dialog::Undo { batch: b.id, lines });
			}
		}
	}

	/// Undo, if the newest batch is still the one the user just looked at.
	fn do_undo(&mut self, expected: &str) {
		let result = self.journal().and_then(|mut j| {
			let newest =
				j.batches().map_err(|e| e.to_string())?.into_iter().rev().find(|b| b.status == BatchStatus::Committed);
			if newest.is_none_or(|b| b.id != expected) {
				return Err("the history changed meanwhile; press Undo again to see what it would revert".into());
			}
			Executor::new(&self.validator(), &mut j, self.state.join("trash"))
				.undo_last()
				.map(|b| match b {
					Some(b) => format!("Undone batch {}: {} operation(s).", b.id, b.steps),
					None => "Nothing to undo.".into(),
				})
				.map_err(|e| one_line(&e))
		});
		self.status = result.unwrap_or_else(|e| format!("Undo failed: {e}"));
		self.reload();
	}

	fn reload(&mut self) {
		self.panels.iter_mut().for_each(Panel::reload);
	}

	/// The roots for an operation: every drive (or `/`), plus the network shares (Windows UNC)
	/// the panels are on — no drive letter covers those.
	fn validator(&self) -> Validator {
		let mut v = self.validator.clone();
		let shares: Vec<PathBuf> = self.panels.iter().filter_map(|p| tpfs_core::share_root(&p.cwd)).collect();
		if !shares.is_empty()
			&& let Ok(with) = Validator::new(self.validator.roots().iter().cloned().chain(shares))
		{
			v = with.with_protected(self.validator.protected().iter().cloned());
		}
		v
	}

	/// Opened per operation, so `tpfs` in another terminal and agents over MCP keep working.
	fn journal(&self) -> Result<Journal, String> {
		Journal::open(self.state.join("journal.jsonl")).map_err(|e| e.to_string())
	}

	pub fn on_mouse(&mut self, m: MouseEvent) {
		if let Some(Dialog::View { offset, lines, .. }) = &mut self.dialog {
			match m.kind {
				MouseEventKind::ScrollUp => *offset = offset.saturating_sub(3),
				MouseEventKind::ScrollDown => *offset = (*offset + 3).min(lines.len().saturating_sub(1)),
				MouseEventKind::Down(MouseButton::Left) => self.dialog = None,
				_ => {}
			}
			return;
		}
		if self.dialog.is_some() {
			if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
				self.dialog = None;
			}
			return;
		}
		let hit = |r: Rect| m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height;

		if let Some(i) = (0..2).find(|&i| hit(self.areas.panels[i])) {
			match m.kind {
				MouseEventKind::ScrollUp => self.panels[i].move_cursor(-3),
				MouseEventKind::ScrollDown => self.panels[i].move_cursor(3),
				MouseEventKind::Down(MouseButton::Left) => {
					self.active = i;
					let area = self.areas.panels[i];
					let row = m.row.saturating_sub(area.y + 1) as usize;
					let index = self.panels[i].offset + row;
					if m.row > area.y && index < self.panels[i].entries.len() {
						let now = Instant::now();
						let double = matches!(self.last_click, Some((t, p, r)) if p == i && r == index && now - t < Duration::from_millis(400));
						self.panels[i].cursor = index;
						self.last_click = Some((now, i, index));
						if double {
							self.panels[i].enter();
							self.last_click = None;
						}
					}
				}
				MouseEventKind::Down(MouseButton::Right) => {
					self.active = i;
					let row = m.row.saturating_sub(self.areas.panels[i].y + 1) as usize;
					let index = self.panels[i].offset + row;
					if index < self.panels[i].entries.len() {
						self.panels[i].cursor = index;
						self.panels[i].toggle_mark();
					}
				}
				_ => {}
			}
		} else if hit(self.areas.fkeys) && matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
			let cell = (self.areas.fkeys.width / 10).max(1);
			let n = ((m.column - self.areas.fkeys.x) / cell).min(9) as u8 + 1;
			self.fkey(n);
		}
	}
}

/// Lower case without the common Latin diacritics, so "zal" finds "zálohy".
pub(crate) fn fold(s: &str) -> String {
	s.chars()
		.flat_map(char::to_lowercase)
		.map(|c| match c {
			'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'ą' => 'a',
			'č' | 'ć' | 'ç' => 'c',
			'ď' => 'd',
			'é' | 'è' | 'ê' | 'ë' | 'ě' | 'ę' => 'e',
			'í' | 'ì' | 'î' | 'ï' => 'i',
			'ľ' | 'ĺ' | 'ł' => 'l',
			'ň' | 'ń' | 'ñ' => 'n',
			'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ő' | 'ø' => 'o',
			'ř' | 'ŕ' => 'r',
			'š' | 'ś' | 'ș' => 's',
			'ť' | 'ț' => 't',
			'ú' | 'ù' | 'û' | 'ü' | 'ů' | 'ű' => 'u',
			'ý' | 'ÿ' => 'y',
			'ž' | 'ź' | 'ż' => 'z',
			c => c,
		})
		.collect()
}

/// `C` for `C:\`, if the path is the root of a drive letter.
fn drive_letter(p: &std::path::Path) -> Option<char> {
	let s = p.to_str()?;
	let b = s.as_bytes();
	(b.len() <= 3 && b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
		.then(|| b[0].to_ascii_uppercase() as char)
}

fn one_line(e: &ExecError) -> String {
	e.to_string().lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
}

/// Whether a plan takes anything away from where it is: trash or folder removal.
pub(crate) fn removes(plan: &Plan) -> bool {
	plan.ops.iter().any(|op| matches!(op, tpfs_core::Op::Trash { .. } | tpfs_core::Op::Rmdir { .. }))
}

#[cfg(test)]
mod tests {
	use std::fs;

	use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

	use super::*;

	/// Draw like the real loop does before each key (the confirmation learns whether it fits).
	fn render(app: &mut App, width: u16, height: u16) -> String {
		let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
		t.draw(|f| crate::ui::draw(f, app)).unwrap();
		let buf = t.backend().buffer();
		(0..height)
			.map(|y| (0..width).map(|x| buf[(x, y)].symbol().to_owned()).collect::<String>())
			.collect::<Vec<_>>()
			.join("\n")
	}

	fn key(app: &mut App, code: KeyCode) {
		render(app, 100, 30);
		app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
	}

	fn ctrl(app: &mut App, c: char) {
		render(app, 100, 30);
		app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
	}

	fn select(app: &mut App, name: &str) {
		let i = app.panels[app.active].entries.iter().position(|e| e.display == name).unwrap();
		app.panels[app.active].cursor = i;
	}

	#[test]
	fn copy_trash_and_undo_through_plans() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		let (left, right, state) = (base.join("left"), base.join("right"), base.join("state"));
		fs::create_dir_all(left.join("folder")).unwrap();
		fs::create_dir(&right).unwrap();
		fs::write(left.join("a.txt"), "A").unwrap();

		let mut app = App::new(left.clone(), right.clone(), state, false).unwrap();

		// F5 previews, Enter applies.
		select(&mut app, "a.txt");
		key(&mut app, KeyCode::F(5));
		assert!(matches!(app.dialog, Some(Dialog::Confirm { .. })));
		assert!(!right.join("a.txt").exists(), "nothing happens before confirmation");
		key(&mut app, KeyCode::Enter);
		assert_eq!(fs::read_to_string(right.join("a.txt")).unwrap(), "A");

		// Trashing a folder is high risk: Enter does nothing, `y` confirms.
		select(&mut app, "folder");
		key(&mut app, KeyCode::F(8));
		key(&mut app, KeyCode::Enter);
		assert!(left.join("folder").exists() && app.dialog.is_some());
		key(&mut app, KeyCode::Char('y'));
		assert!(!left.join("folder").exists());

		// Undo twice restores both; each asks first and names the batch.
		ctrl(&mut app, 'z');
		assert!(matches!(app.dialog, Some(Dialog::Undo { .. })));
		assert!(!left.join("folder").is_dir(), "nothing is undone before confirming");
		key(&mut app, KeyCode::Char('y'));
		assert!(left.join("folder").is_dir());
		key(&mut app, KeyCode::F(9));
		key(&mut app, KeyCode::Char('y'));
		assert!(!right.join("a.txt").exists());
	}

	#[test]
	fn a_stray_enter_never_trashes_anything() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		let (left, right, state) = (base.join("left"), base.join("right"), base.join("state"));
		fs::create_dir_all(&left).unwrap();
		fs::create_dir(&right).unwrap();
		fs::write(left.join("a.txt"), "A").unwrap();
		let mut app = App::new(left.clone(), right, state, false).unwrap();
		select(&mut app, "a.txt");
		key(&mut app, KeyCode::F(8)); // one file to the trash: low risk, but it removes something
		key(&mut app, KeyCode::Enter);
		key(&mut app, KeyCode::Enter);
		assert!(left.join("a.txt").exists() && app.dialog.is_some(), "Enter trashed the file");
		key(&mut app, KeyCode::Char('y'));
		assert!(!left.join("a.txt").exists());
	}

	#[test]
	fn copy_onto_an_existing_file_is_refused_in_the_dialog() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir_all(base.join("l")).unwrap();
		fs::create_dir_all(base.join("r")).unwrap();
		fs::write(base.join("l/x.txt"), "new").unwrap();
		fs::write(base.join("r/x.txt"), "old").unwrap();

		let mut app = App::new(base.join("l"), base.join("r"), base.join("state"), false).unwrap();
		select(&mut app, "x.txt");
		key(&mut app, KeyCode::F(5));
		match &app.dialog {
			Some(Dialog::Confirm { issues, .. }) => assert_eq!(issues.len(), 1),
			_ => panic!("expected a confirmation dialog"),
		}
		key(&mut app, KeyCode::Enter);
		assert_eq!(fs::read_to_string(base.join("r/x.txt")).unwrap(), "old");
	}

	#[test]
	fn mkdir_and_rename_by_typing() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir_all(base.join("l")).unwrap();
		let mut app = App::new(base.join("l"), base.join("l"), base.join("state"), false).unwrap();

		key(&mut app, KeyCode::F(7));
		for c in "nová".chars() {
			key(&mut app, KeyCode::Char(c));
		}
		key(&mut app, KeyCode::Enter);
		assert!(base.join("l/nová").is_dir());

		select(&mut app, "nová");
		key(&mut app, KeyCode::F(2));
		for _ in 0..4 {
			key(&mut app, KeyCode::Backspace);
		}
		for c in "archiv".chars() {
			key(&mut app, KeyCode::Char(c));
		}
		key(&mut app, KeyCode::Enter);
		assert!(base.join("l/archiv").is_dir() && !base.join("l/nová").exists());
	}

	#[test]
	fn viewer_and_command_line() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir_all(base.join("l")).unwrap();
		let text: String = (1..=50).map(|i| format!("line {i}\n")).collect();
		fs::write(base.join("l/notes.txt"), text).unwrap();
		let mut app = App::new(base.join("l"), base.join("l"), base.join("state"), false).unwrap();

		// Enter on a file opens the viewer; it scrolls and closes with Esc.
		select(&mut app, "notes.txt");
		key(&mut app, KeyCode::Enter);
		key(&mut app, KeyCode::PageDown);
		match &app.dialog {
			Some(Dialog::View { lines, offset, .. }) => assert_eq!((lines.len(), *offset), (50, 20)),
			_ => panic!("expected the viewer"),
		}
		key(&mut app, KeyCode::Esc);
		assert!(app.dialog.is_none());

		// `:` opens the command line; Enter hands the command to the terminal runner.
		key(&mut app, KeyCode::Char(':'));
		for c in "ls -l".chars() {
			key(&mut app, KeyCode::Char(c));
		}
		key(&mut app, KeyCode::Enter);
		match &app.external {
			Some(External::Shell { command, cwd }) => {
				assert_eq!((command.as_str(), cwd.as_path()), ("ls -l", base.join("l").as_path()))
			}
			_ => panic!("expected a shell command"),
		}

		// F4 asks for the editor.
		app.external = None;
		key(&mut app, KeyCode::F(4));
		assert!(matches!(&app.external, Some(External::Edit(p)) if p.ends_with("notes.txt")));
	}

	#[test]
	fn drive_picker_switches_a_panel() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir_all(base.join("l")).unwrap();
		let mut app = App::new(base.join("l"), base.join("l"), base.join("state"), false).unwrap();

		app.on_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::ALT));
		let Some(Dialog::Drives { panel, items, .. }) = &app.dialog else { panic!("no drive list") };
		assert_eq!(*panel, 1);
		let root = items[0].clone();
		key(&mut app, KeyCode::Home);
		key(&mut app, KeyCode::Enter);
		assert!(app.dialog.is_none());
		assert_eq!((app.active, &app.panels[1].cwd), (1, &root));
		assert_eq!(app.panels[0].cwd, base.join("l"));

		// At the top of a drive, going up offers the drive list.
		key(&mut app, KeyCode::Backspace);
		assert!(matches!(app.dialog, Some(Dialog::Drives { panel: 1, .. })));
		key(&mut app, KeyCode::Esc);
		assert!(app.dialog.is_none());
	}

	#[test]
	fn drive_letters() {
		assert_eq!(drive_letter(std::path::Path::new("C:\\")), Some('C'));
		assert_eq!(drive_letter(std::path::Path::new("d:")), Some('D'));
		assert_eq!(drive_letter(std::path::Path::new("/")), None);
		assert_eq!(drive_letter(std::path::Path::new("C:\\Users")), None);
	}

	#[test]
	fn quick_search_jumps_ignoring_case_and_diacritics() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		for name in ["alfa.txt", "Zálohy", "zebra.txt"] {
			fs::write(base.join(name), "x").unwrap();
		}
		let mut app = App::new(base.clone(), base.clone(), base.join("state"), false).unwrap();
		let name = |app: &App| app.panels[0].current().unwrap().display.clone();

		key(&mut app, KeyCode::Char('z'));
		key(&mut app, KeyCode::Char('A'));
		assert_eq!(name(&app), "Zálohy");
		key(&mut app, KeyCode::Backspace);
		key(&mut app, KeyCode::Char('e'));
		assert_eq!(name(&app), "zebra.txt");
		assert!(app.quick.as_ref().is_some_and(|q| q.text == "ze" && !q.miss));

		key(&mut app, KeyCode::Char('x'));
		assert!(app.quick.as_ref().is_some_and(|q| q.miss));
		assert_eq!(name(&app), "zebra.txt", "a miss keeps the cursor");
		key(&mut app, KeyCode::Esc);
		assert!(app.quick.is_none());
		assert_eq!(fold("ŘEŽ Ůl"), "rez ul");
	}

	#[test]
	fn shift_f2_syncs_active_panel_to_the_other() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir_all(base.join("l/album")).unwrap();
		fs::create_dir_all(base.join("r")).unwrap();
		fs::write(base.join("l/album/a.jpg"), "a").unwrap();
		fs::write(base.join("r/extra.txt"), "x").unwrap();
		let mut app = App::new(base.join("l"), base.join("r"), base.join("state"), false).unwrap();

		app.on_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::SHIFT));
		assert!(matches!(&app.dialog, Some(Dialog::Sync { cmp, .. }) if cmp.items.len() == 2));
		key(&mut app, KeyCode::Char('m'));
		assert!(matches!(&app.dialog, Some(Dialog::Sync { opts, .. }) if opts.mirror));
		key(&mut app, KeyCode::Char('m'));
		key(&mut app, KeyCode::Enter);
		assert!(matches!(&app.dialog, Some(Dialog::Confirm { plan, .. }) if plan.ops.len() == 1));
		key(&mut app, KeyCode::Enter);
		assert_eq!(fs::read_to_string(base.join("r/album/a.jpg")).unwrap(), "a");
		assert!(base.join("r/extra.txt").exists(), "mirror was switched off again");

		ctrl(&mut app, 'z');
		key(&mut app, KeyCode::Char('y'));
		assert!(!base.join("r/album").exists());
	}

	fn confirm_app(ops: Vec<tpfs_core::Op>) -> (tempfile::TempDir, App) {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir(base.join("l")).unwrap();
		let mut app = App::new(base.join("l"), base.join("l"), base.join("state"), false).unwrap();
		app.propose(Plan::new(ops, "test"));
		(tmp, app)
	}

	fn long_moves(n: usize) -> Vec<tpfs_core::Op> {
		let dir = PathBuf::from("/home/u/Fotky/2024/Dovolená v Chorvatsku/originály");
		(0..n)
			.map(|i| tpfs_core::Op::Move {
				from: dir.join(format!("IMG_{i:04}.jpg")),
				to: dir.join("vybrané/nejlepší z dovolené").join(format!("IMG_{i:04}.jpg")),
			})
			.collect()
	}

	#[test]
	fn confirmation_shows_names_risk_and_keys_at_80x24_and_60x16() {
		for (w, h) in [(80, 24), (60, 16)] {
			let (_t, mut app) = confirm_app(long_moves(30));
			let screen = render(&mut app, w, h);
			assert!(screen.contains("IMG_0000.jpg"), "{w}x{h}:\n{screen}");
			assert!(screen.contains("→"), "{w}x{h}: destination not shown\n{screen}");
			assert!(screen.contains("Risk:"), "{w}x{h}: risk not shown\n{screen}");
			assert!(screen.contains("Esc"), "{w}x{h}: keys not shown\n{screen}");
		}
	}

	#[test]
	fn medium_risk_is_not_approved_by_enter() {
		let (_t, mut app) = confirm_app(long_moves(60)); // many operations: at least medium
		assert!(matches!(&app.dialog, Some(Dialog::Confirm { risk, .. }) if risk.level != risk::Level::Low));
		key(&mut app, KeyCode::Enter);
		assert!(matches!(app.dialog, Some(Dialog::Confirm { .. })), "Enter applied a medium/high plan");
	}

	#[test]
	fn high_risk_needs_the_whole_list_read_and_a_big_enough_terminal() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir_all(base.join("l/folder")).unwrap();
		let mut ops = vec![tpfs_core::Op::Trash { path: base.join("l/folder") }];
		ops.extend(long_moves(20));
		let mut app = App::new(base.join("l"), base.join("l"), base.join("state"), false).unwrap();
		app.propose(Plan::new(ops, ""));
		assert!(matches!(&app.dialog, Some(Dialog::Confirm { risk, .. }) if risk.level == risk::Level::High));

		// Too small: nothing can be approved.
		let screen = render(&mut app, 30, 8);
		assert!(screen.contains("too small") || !screen.contains("Proceed"), "{screen}");
		app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
		assert!(matches!(app.dialog, Some(Dialog::Confirm { .. })));

		// Normal size, but the list was not read to the end yet.
		render(&mut app, 80, 24);
		app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
		assert!(matches!(app.dialog, Some(Dialog::Confirm { .. })), "approved without reading the list");

		// After scrolling to the end, `y` is accepted (here it fails validation-free paths, so the
		// dialog goes away either way: the plan was handed to the executor).
		app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
		render(&mut app, 80, 24);
		app.on_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
		assert!(!matches!(app.dialog, Some(Dialog::Confirm { issues, .. }) if issues.is_empty()));
	}

	#[test]
	fn wide_and_combining_names_keep_columns_aligned() {
		use crate::ui::tests_support::{pad_for_test as pad, width_for_test as width};
		for name in ["plain.txt", "日本語.txt", "e\u{301}cole.txt"] {
			assert_eq!(width(&pad(name, 20)), 20, "{name}");
		}
	}

	#[test]
	fn keys_stay_visible_for_valid_scrolling_plans_at_any_size() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir_all(base.join("l/into")).unwrap();
		fs::create_dir_all(base.join("l/folder")).unwrap();
		let mut ops: Vec<tpfs_core::Op> = (0..25)
			.map(|i| {
				let f = base.join(format!("l/photo-{i:02}.jpg"));
				fs::write(&f, "x").unwrap();
				tpfs_core::Op::Move { from: f, to: base.join(format!("l/into/photo-{i:02}.jpg")) }
			})
			.collect();
		for high in [false, true] {
			if high {
				ops.push(tpfs_core::Op::Trash { path: base.join("l/folder") });
			}
			for (w, h) in [(80, 24), (100, 30), (60, 16), (40, 12)] {
				let mut app = App::new(base.join("l"), base.join("l"), base.join("state"), false).unwrap();
				app.propose(Plan::new(ops.clone(), ""));
				assert!(matches!(&app.dialog, Some(Dialog::Confirm { issues, .. }) if issues.is_empty()));
				for end in [false, true] {
					if end {
						app.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
					}
					let screen = render(&mut app, w, h);
					let fits = matches!(&app.dialog, Some(Dialog::Confirm { fits: true, .. }));
					if fits {
						assert!(screen.contains("Esc"), "{w}x{h} high={high} end={end}: keys cut off\n{screen}");
						assert!(
							screen.contains("Proceed") || screen.contains("read the whole list"),
							"{w}x{h} high={high} end={end}\n{screen}"
						);
					}
				}
			}
		}
	}

	#[test]
	fn a_new_folder_name_cannot_point_elsewhere() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir_all(base.join("l")).unwrap();
		let mut app = App::new(base.join("l"), base.join("l"), base.join("state"), false).unwrap();
		for name in ["../escaped", "/abs/x", "a/b", ".."] {
			app.submit_input(name, &InputKind::Mkdir);
			assert!(app.dialog.is_none(), "{name}");
		}
		assert!(!base.join("escaped").exists() && !base.join("l/a").exists());
		app.submit_input("fine", &InputKind::Mkdir);
		assert!(base.join("l/fine").is_dir());
	}

	#[test]
	fn undo_over_an_interrupted_batch_points_to_recover() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir_all(base.join("l")).unwrap();
		let mut app = App::new(base.join("l"), base.join("l"), base.join("state"), false).unwrap();
		let mut j = app.journal().unwrap();
		j.append(&tpfs_core::Entry::Begin {
			batch: "x".into(),
			at: 0,
			source: "human".into(),
			reason: String::new(),
			digest: String::new(),
			v: 1,
		})
		.unwrap();
		drop(j);
		ctrl(&mut app, 'z');
		assert!(app.dialog.is_none() && app.status.contains("tpfs recover"), "{}", app.status);
	}

	#[test]
	fn undo_dialog_keys_stay_visible_in_small_terminals() {
		let tmp = tempfile::TempDir::new().unwrap();
		let base = tpfs_core::canonicalize(tmp.path()).unwrap();
		fs::create_dir_all(base.join("l")).unwrap();
		let mut app = App::new(base.join("l"), base.join("l"), base.join("state"), false).unwrap();
		let ops: Vec<Op> = (0..20).map(|i| Op::Mkdir { path: base.join(format!("l/d{i:02}")) }).collect();
		app.execute(&Plan::new(ops, "many"));
		app.undo();
		assert!(matches!(app.dialog, Some(Dialog::Undo { .. })));
		for (w, h) in [(80, 24), (80, 16), (60, 12)] {
			let screen = render(&mut app, w, h);
			assert!(screen.contains("Esc"), "{w}x{h}\n{screen}");
		}
	}
}
