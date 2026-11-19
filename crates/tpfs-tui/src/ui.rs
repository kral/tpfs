//! Drawing in the Norton Commander palette: blue panes, cyan double frames, yellow marks,
//! a cyan cursor bar, grey dialogs, and the F-key bar at the bottom. 16 ANSI colours only.

use ratatui::{
	Frame,
	layout::{Constraint, Layout, Rect},
	style::{Color, Modifier, Style},
	text::{Line, Span},
	widgets::{Block, BorderType, Clear, Paragraph},
};

use crate::{
	app::{App, Dialog, FKEYS},
	panel::Panel,
};

const PANE: Style = Style::new().fg(Color::LightCyan).bg(Color::Blue);
const FRAME: Style = Style::new().fg(Color::Cyan).bg(Color::Blue);
const TITLE_ACTIVE: Style = Style::new().fg(Color::Black).bg(Color::Cyan);
const DIR: Style = Style::new().fg(Color::White).bg(Color::Blue).add_modifier(Modifier::BOLD);
const MARKED: Style = Style::new().fg(Color::Yellow).bg(Color::Blue).add_modifier(Modifier::BOLD);
const CURSOR: Style = Style::new().fg(Color::Black).bg(Color::Cyan);
const CURSOR_MARKED: Style = Style::new().fg(Color::Black).bg(Color::Yellow);
const DIALOG: Style = Style::new().fg(Color::Black).bg(Color::Gray);
const KEY_NUM: Style = Style::new().fg(Color::White).bg(Color::Black);
const KEY_LABEL: Style = Style::new().fg(Color::Black).bg(Color::Cyan);

pub(crate) fn draw(f: &mut Frame, app: &mut App) {
	let [main, status, fkeys] =
		Layout::vertical([Constraint::Fill(1), Constraint::Length(1), Constraint::Length(1)]).areas(f.area());
	let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(main);

	app.areas.panels = [left, right];
	app.areas.fkeys = fkeys;

	for (i, area) in [left, right].into_iter().enumerate() {
		let active = i == app.active;
		draw_panel(f, &mut app.panels[i], area, active, app.icons);
	}

	// The status line doubles as the command line (opened with `:`).
	let status_line = match &app.command {
		Some(cmd) => {
			let cwd = tpfs_core::sanitize(app.panels[app.active].cwd.as_os_str());
			Line::from(vec![
				Span::styled(format!("{cwd}> "), Style::new().fg(Color::Gray).bg(Color::Black)),
				Span::styled(cmd.clone(), Style::new().fg(Color::White).bg(Color::Black)),
				Span::styled("_", Style::new().fg(Color::Yellow).bg(Color::Black)),
			])
		}
		None => match &app.quick {
			Some(q) => Line::from(vec![
				Span::styled(" Search: ", Style::new().fg(Color::Gray).bg(Color::Black)),
				Span::styled(
					q.text.clone(),
					Style::new().fg(Color::Black).bg(if q.miss { Color::Red } else { Color::Cyan }),
				),
				Span::raw(if q.miss { "  no match" } else { "" }),
			]),
			None => Line::raw(format!(" {}", app.status)),
		},
	};
	f.render_widget(Paragraph::new(status_line).style(Style::new().fg(Color::Gray).bg(Color::Black)), status);
	draw_fkeys(f, fkeys);

	if let Some(Dialog::Confirm { plan, risk, issues, offset, fits, seen_all }) = &mut app.dialog {
		let view = draw_confirm(f, plan, risk, issues, *offset);
		*offset = view.offset;
		*fits = view.fits;
		*seen_all |= view.at_end;
		return;
	}
	match &app.dialog {
		Some(Dialog::Confirm { .. }) => {}

		Some(Dialog::Input { title, value, .. }) => draw_input(f, title, value),
		Some(Dialog::Help) => draw_help(f),
		Some(Dialog::Sync { cmp, opts, offset }) => draw_sync(f, app, cmp, *opts, *offset),
		Some(Dialog::Drives { panel, items, cursor }) => draw_drives(f, *panel, items, *cursor),
		Some(Dialog::View { title, lines, offset }) => draw_view(f, main, title, lines, *offset),
		None => {}
	}
}

fn draw_panel(f: &mut Frame, panel: &mut Panel, area: Rect, active: bool, icons: bool) {
	let path = tpfs_core::sanitize(panel.cwd.as_os_str());
	let width = area.width.saturating_sub(4) as usize;
	let title = format!(" {} ", truncate_left(&path, width));
	let footer = if !panel.marked.is_empty() {
		Span::styled(format!(" {} selected, {} ", panel.marked.len(), human(panel.marked_size())), MARKED)
	} else if let Some(e) = &panel.error {
		Span::styled(format!(" {} ", truncate_left(e, width)), Style::new().fg(Color::LightRed).bg(Color::Blue))
	} else {
		let info = panel
			.current()
			.map(|e| {
				if e.is_up {
					"<UP-DIR>".into()
				} else if e.is_dir {
					"<DIR>".into()
				} else {
					human(e.size)
				}
			})
			.unwrap_or_default();
		Span::styled(format!(" {info} "), FRAME)
	};

	let block = Block::bordered()
		.border_type(BorderType::Double)
		.border_style(FRAME)
		.style(PANE)
		.title(Span::styled(title, if active { TITLE_ACTIVE } else { FRAME }))
		.title_bottom(Line::from(footer).centered());
	let inner = block.inner(area);
	f.render_widget(block, area);

	let height = inner.height as usize;
	panel.scroll_into_view(height);
	let w = inner.width as usize;
	let size_w = 9;
	// Columns appear as the panel grows: Name+Size, then Date, then Time.
	let show_date = w >= 40;
	let show_time = w >= 56;
	let date_w = if show_time {
		15
	} else if show_date {
		9
	} else {
		0
	};

	let lines: Vec<Line> = panel
		.entries
		.iter()
		.enumerate()
		.skip(panel.offset)
		.take(height)
		.map(|(i, e)| {
			let marked = panel.marked.contains(&e.name);
			let mut name = if e.is_dir && !e.is_up { format!("{}/", e.display) } else { e.display.clone() };
			if icons {
				name = format!("{} {name}", crate::extra::icon(&e.display, e.is_dir, e.is_up));
			}
			let size = if e.is_up {
				"<UP>".to_owned()
			} else if e.is_dir {
				"<DIR>".to_owned()
			} else {
				human(e.size)
			};
			let date = match (e.modified, show_date) {
				(Some(t), true) => {
					let (d, t) = crate::extra::format_time(t);
					if show_time { format!(" {d} {t}") } else { format!(" {d}") }
				}
				_ => " ".repeat(date_w),
			};
			let name_w = w.saturating_sub(size_w + 1 + date_w);
			let text = format!("{}{:>size_w$}{date}", pad(&truncate_right(&name, name_w), name_w + 1), size);

			let style = match (i == panel.cursor && active, marked) {
				(true, true) => CURSOR_MARKED,
				(true, false) => CURSOR,
				(false, true) => MARKED,
				(false, false) if e.is_dir => DIR,
				_ => PANE,
			};
			Line::styled(text, style)
		})
		.collect();
	f.render_widget(Paragraph::new(lines), inner);
}

/// F3 viewer: full panel area, black on cyan title, plain text.
fn draw_view(f: &mut Frame, area: Rect, title: &str, lines: &[String], offset: usize) {
	let block = Block::bordered()
		.border_type(BorderType::Double)
		.border_style(FRAME)
		.style(PANE)
		.title(Span::styled(format!(" {title} "), TITLE_ACTIVE))
		.title_bottom(
			Line::from(Span::styled(
				format!(" {}/{} · ↑↓ PgUp PgDn · Esc/F3 close ", (offset + 1).min(lines.len()), lines.len()),
				FRAME,
			))
			.centered(),
		);
	let inner = block.inner(area);
	f.render_widget(Clear, area);
	f.render_widget(block, area);
	let text: Vec<Line> = lines.iter().skip(offset).take(inner.height as usize).map(|l| Line::raw(l.clone())).collect();
	f.render_widget(Paragraph::new(text).style(Style::new().fg(Color::White).bg(Color::Blue)), inner);
}

fn draw_fkeys(f: &mut Frame, area: Rect) {
	let cell = (area.width / 10).max(1) as usize;
	let mut spans = Vec::new();
	for (i, label) in FKEYS.iter().enumerate() {
		let num = (i + 1).to_string();
		let w = if i == 9 { (area.width as usize).saturating_sub(cell * 9) } else { cell };
		spans.push(Span::styled(num.clone(), KEY_NUM));
		spans.push(Span::styled(
			pad(&truncate_right(label, w.saturating_sub(num.len())), w.saturating_sub(num.len())),
			KEY_LABEL,
		));
	}
	f.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(crate) struct ConfirmView {
	pub offset: usize,
	pub fits: bool,
	pub at_end: bool,
}

/// One operation as one or two lines that always show the file names: long paths are shortened
/// in the middle, never cut off at the end.
fn op_lines(op: &tpfs_core::Op, width: usize) -> Vec<String> {
	use tpfs_core::Op;
	let s = |p: &std::path::Path| tpfs_core::sanitize(p.as_os_str());
	let line = |verb: &str, text: String| format!("{verb:<7}{}", truncate_middle(&text, width.saturating_sub(7)));
	match op {
		Op::Move { from, to } => vec![line("MOVE", s(from)), line("   →", s(to))],
		Op::Copy { from, to } => vec![line("COPY", s(from)), line("   →", s(to))],
		Op::Rename { path, new_name } => {
			vec![line("RENAME", s(path)), line("   →", tpfs_core::sanitize(new_name.as_ref()))]
		}
		Op::Trash { path } => vec![line("TRASH", s(path))],
		Op::Mkdir { path } => vec![line("MKDIR", s(path))],
		Op::Rmdir { path } => vec![line("RMDIR", s(path))],
		other => vec![truncate_middle(&other.to_string(), width)],
	}
}

/// Totals per kind of operation, e.g. "3 move · 1 trash".
fn totals(plan: &tpfs_core::Plan) -> String {
	use tpfs_core::Op;
	let mut counts = [0usize; 6];
	for op in &plan.ops {
		counts[match op {
			Op::Move { .. } => 0,
			Op::Copy { .. } => 1,
			Op::Rename { .. } => 2,
			Op::Trash { .. } => 3,
			Op::Mkdir { .. } => 4,
			Op::Rmdir { .. } => 5,
			_ => continue,
		}] += 1;
	}
	let names = ["move", "copy", "rename", "trash", "mkdir", "rmdir"];
	counts.iter().zip(names).filter(|(n, _)| **n > 0).map(|(n, k)| format!("{n} {k}")).collect::<Vec<_>>().join(" · ")
}

/// The confirmation before anything changes. The risk, the totals and the keys are always on
/// screen; the operation list scrolls in the space left. If even that does not fit, nothing can be
/// approved and the dialog says so.
fn draw_confirm(
	f: &mut Frame,
	plan: &tpfs_core::Plan,
	risk: &tpfs_core::risk::Risk,
	issues: &[tpfs_core::Issue],
	offset: usize,
) -> ConfirmView {
	use tpfs_core::risk::Level;

	let area = f.area();
	let rect = Rect {
		x: area.x + 1.min(area.width),
		y: area.y + 1.min(area.height),
		width: area.width.saturating_sub(2),
		height: area.height.saturating_sub(2),
	};
	let w = rect.width.saturating_sub(4) as usize;
	let frame = if risk.level == Level::High { Color::Red } else { Color::Black };

	let mut head: Vec<Line> = Vec::new();
	if !plan.reason.is_empty() {
		head.push(Line::styled(
			truncate_right(&tpfs_core::sanitize(plan.reason.as_ref()), w),
			DIALOG.add_modifier(Modifier::BOLD),
		));
	}

	let (risk_style, risk_text) = match risk.level {
		Level::Low => (DIALOG, "Risk: low".to_owned()),
		Level::Medium => {
			(DIALOG.fg(Color::Blue).add_modifier(Modifier::BOLD), format!("Risk: MEDIUM — {}", risk.reasons.join("; ")))
		}
		_ => (DIALOG.fg(Color::Red).add_modifier(Modifier::BOLD), format!("Risk: HIGH — {}", risk.reasons.join("; "))),
	};
	let mut foot: Vec<Line> = vec![Line::styled(truncate_right(&format!("Total: {}", totals(plan)), w), DIALOG)];
	foot.extend(wrap(&risk_text, w).into_iter().map(|l| Line::styled(l, risk_style)));
	// Only the visible window is formatted: a 10 000-operation plan costs nothing per redraw.
	let line_count = |op: &tpfs_core::Op| match op {
		tpfs_core::Op::Move { .. } | tpfs_core::Op::Copy { .. } | tpfs_core::Op::Rename { .. } => 2,
		_ => 1,
	};
	let total_lines: usize = plan.ops.iter().map(line_count).sum();

	// Space for the list after the fixed parts; problems get at most a third of what is left.
	let free = (rect.height as usize).saturating_sub(2 + head.len() + foot.len() + 3);
	let shown_issues = issues.len().min(6).min((free / 3).max(1));
	let rows = free.saturating_sub(shown_issues + usize::from(issues.len() > shown_issues));
	let fits = w >= 20 && rows >= 2.min(total_lines.max(1));
	let max_offset = total_lines.saturating_sub(rows.max(1));
	let offset = offset.min(max_offset);
	let at_end = fits && offset + rows >= total_lines;
	let window: Vec<String> = {
		let mut out = Vec::with_capacity(rows);
		let mut line = 0;
		for op in &plan.ops {
			let n = line_count(op);
			if line + n > offset {
				for (k, l) in op_lines(op, w).into_iter().enumerate() {
					if line + k >= offset && out.len() < rows {
						out.push(l);
					}
				}
			}
			line += n;
			if out.len() >= rows {
				break;
			}
		}
		out
	};

	let buttons = if !issues.is_empty() {
		"[ Esc: Close ]".to_owned()
	} else if !fits {
		"Terminal too small to review this plan — enlarge it.   [ Esc: Cancel ]".to_owned()
	} else {
		match risk.level {
			Level::Low => "[ Enter: Proceed ]   [ Esc: Cancel ]".to_owned(),
			Level::Medium => "[ Y: Proceed ]   [ Esc: Cancel ]".to_owned(),
			_ if at_end => "[ Y: Proceed ]   [ Esc: Cancel ]".to_owned(),
			_ => "↓ PgDn: read the whole list to proceed   [ Esc: Cancel ]".to_owned(),
		}
	};
	if issues.is_empty() {
		foot.push(Line::raw(truncate_right("Nothing is overwritten; Ctrl+Z undoes it.", w)));
	} else {
		for i in issues.iter().take(shown_issues) {
			foot.push(Line::styled(truncate_middle(&format!("✗ {i}"), w), DIALOG.fg(Color::Red)));
		}
		if issues.len() > shown_issues {
			foot.push(Line::styled(
				format!("… {} more problem(s)", issues.len() - shown_issues),
				DIALOG.fg(Color::Red),
			));
		}
	}

	let mut lines = head;
	if fits {
		lines.extend(window.into_iter().map(Line::raw));
		if total_lines > rows {
			lines.push(Line::styled(
				format!("lines {}–{} of {} (↑↓ PgUp PgDn)", offset + 1, (offset + rows).min(total_lines), total_lines),
				DIALOG.add_modifier(Modifier::DIM),
			));
		} else {
			lines.push(Line::raw(""));
		}
	}
	lines.extend(foot);
	lines.push(Line::raw(""));
	lines.push(Line::raw(truncate_right(&buttons, w)).centered());

	let title = format!(" {} operation(s) ", plan.ops.len());
	f.render_widget(Clear, rect);
	let block = Block::bordered()
		.border_type(BorderType::Double)
		.border_style(DIALOG.fg(frame))
		.style(DIALOG)
		.title(Line::styled(title, DIALOG.fg(frame).add_modifier(Modifier::BOLD)).centered());
	let inner = block.inner(rect);
	f.render_widget(block, rect);
	f.render_widget(
		Paragraph::new(lines).style(DIALOG),
		inner.inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 }),
	);
	ConfirmView { offset, fits, at_end }
}

/// Word-wrap to `width` display columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
	let mut out = vec![String::new()];
	for word in text.split(' ') {
		let last = out.last_mut().expect("never empty");
		if !last.is_empty() && width_of(last) + 1 + width_of(word) > width {
			out.push(String::new());
		}
		let last = out.last_mut().expect("never empty");
		if !last.is_empty() {
			last.push(' ');
		}
		last.push_str(&truncate_middle(word, width));
	}
	out
}

fn draw_input(f: &mut Frame, title: &str, value: &str) {
	let lines = vec![
		Line::raw(""),
		Line::from(vec![
			Span::styled(format!(" {value}"), Style::new().fg(Color::White).bg(Color::Blue)),
			Span::styled("_", Style::new().fg(Color::Yellow).bg(Color::Blue)),
		]),
		Line::raw(""),
		Line::raw("[ Enter: OK ]   [ Esc: Cancel ]").centered(),
	];
	dialog(f, &format!(" {title} "), lines, Color::Black);
}

fn draw_sync(
	f: &mut Frame,
	app: &App,
	cmp: &tpfs_core::sync::Comparison,
	opts: tpfs_core::sync::SyncOptions,
	offset: usize,
) {
	use tpfs_core::sync::{Action, Direction, action};
	let s = |p: &std::path::Path| tpfs_core::sanitize(p.as_os_str());
	let (l, r) = (s(&app.panels[0].cwd), s(&app.panels[1].cwd));
	let arrow = match opts.direction {
		Direction::LeftToRight => "→",
		Direction::RightToLeft => "←",
		Direction::Both => "↔",
	};
	let width = (f.area().width as usize).saturating_sub(10).clamp(40, 100);
	let shown = (f.area().height as usize).saturating_sub(14).clamp(3, 14);

	let mut lines = vec![
		Line::styled(truncate_left(&format!("{l}  {arrow}  {r}"), width), DIALOG.add_modifier(Modifier::BOLD)),
		Line::raw(format!("{} difference(s), {} identical file(s)", cmp.items.len(), cmp.same)),
		Line::raw(""),
	];
	if cmp.items.is_empty() {
		lines.push(Line::raw("The folders are identical."));
	}
	let planned = cmp.items.iter().filter(|i| !matches!(action(i, opts), Action::Skip(_))).count();
	for item in cmp.items.iter().skip(offset).take(shown) {
		let a = action(item, opts);
		let style = match a {
			Action::Skip(_) => DIALOG.fg(Color::DarkGray),
			Action::TrashLeft | Action::TrashRight => DIALOG.fg(Color::Red),
			Action::ReplaceLeft | Action::ReplaceRight => DIALOG.fg(Color::Blue),
			_ => DIALOG,
		};
		let dir = item.left.as_ref().or(item.right.as_ref()).is_some_and(|m| m.kind == tpfs_core::sync::Kind::Dir);
		let name = format!("{}{}", s(&item.rel), if dir { "/" } else { "" });
		lines.push(Line::from(vec![
			Span::styled(format!(" {:<12}", a.label()), style.add_modifier(Modifier::BOLD)),
			Span::styled(truncate_left(&name, width.saturating_sub(14)), style),
		]));
	}
	if cmp.items.len() > offset + shown {
		lines.push(Line::raw(format!(" … {} more (↑/↓)", cmp.items.len() - offset - shown)));
	}
	lines.push(Line::raw(""));
	let mirror = match (opts.direction, opts.mirror) {
		(Direction::Both, _) => "mirror: n/a",
		(_, true) => "mirror: ON (target-only items go to the trash)",
		(_, false) => "mirror: off",
	};
	lines.push(Line::raw(format!("←/→ direction · B both ways · M {mirror}")));
	lines.push(Line::raw(""));
	lines.push(Line::raw(format!("[ Enter: Review plan ({planned}) ]   [ Esc: Cancel ]")).centered());
	dialog(
		f,
		" Compare & sync ",
		lines,
		if opts.mirror && opts.direction != Direction::Both { Color::Red } else { Color::Black },
	);
}

fn draw_drives(f: &mut Frame, panel: usize, items: &[std::path::PathBuf], cursor: usize) {
	let width = items.iter().map(|d| d.as_os_str().len()).max().unwrap_or(0).max(24);
	let mut lines: Vec<Line> = items
		.iter()
		.enumerate()
		.map(|(i, d)| {
			let text = pad(&format!(" {}", tpfs_core::sanitize(d.as_os_str())), width + 2);
			if i == cursor { Line::styled(text, CURSOR) } else { Line::raw(text) }
		})
		.collect();
	lines.push(Line::raw(""));
	lines.push(Line::raw("[ Enter: Go ]   [ Esc: Cancel ]").centered());
	let title = if panel == 0 { " Left panel: drive " } else { " Right panel: drive " };
	dialog(f, title, lines, Color::Black);
}

fn draw_help(f: &mut Frame) {
	let rows = [
		("Tab", "switch panel"),
		("Enter / →", "enter directory"),
		("Backspace / ←", "parent directory (drive list at the top)"),
		("Alt+F1 / Alt+F2", "drive for the left / right panel"),
		("Shift+F2", "compare & sync the two folders"),
		("Ins / Space / right click", "mark"),
		("F2", "rename"),
		("F3 / Enter on a file", "view"),
		("F4", "edit in $EDITOR"),
		(":", "command line (runs in this folder, not journaled)"),
		("letters", "quick search: jump to a name"),
		("F5 / F6", "copy / move to the other panel"),
		("F7", "make directory"),
		("F8 / Del", "move to trash"),
		("F9 / Ctrl+Z", "undo the last change"),
		("F10 / Ctrl+Q", "quit"),
		("Mouse", "click, double click, wheel, F-key bar"),
	];
	let mut lines: Vec<Line> = rows
		.iter()
		.map(|(k, d)| {
			Line::from(vec![Span::styled(format!(" {k:<26}"), DIALOG.add_modifier(Modifier::BOLD)), Span::raw(*d)])
		})
		.collect();
	lines.push(Line::raw(""));
	lines.push(Line::raw("Every change is a plan: previewed, journaled, undoable.").centered());
	dialog(f, " tpfs commander ", lines, Color::Black);
}

fn dialog(f: &mut Frame, title: &str, lines: Vec<Line>, frame: Color) {
	let area = f.area();
	let width = lines
		.iter()
		.map(Line::width)
		.max()
		.unwrap_or(0)
		.max(title.chars().count() + 4)
		.saturating_add(4)
		.min(area.width.saturating_sub(4) as usize) as u16;
	let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
	let rect = Rect {
		x: area.x + (area.width.saturating_sub(width)) / 2,
		y: area.y + (area.height.saturating_sub(height)) / 2,
		width,
		height,
	};

	// Shadow, like Turbo Vision.
	let shadow = Rect { x: rect.x + 2, y: rect.y + 1, width: rect.width, height: rect.height }.intersection(area);
	f.render_widget(Block::new().style(Style::new().bg(Color::Black)), shadow);

	f.render_widget(Clear, rect);
	let block = Block::bordered()
		.border_type(BorderType::Double)
		.border_style(DIALOG.fg(frame))
		.style(DIALOG)
		.title(Line::styled(title, DIALOG.fg(frame).add_modifier(Modifier::BOLD)).centered());
	let inner = block.inner(rect);
	f.render_widget(block, rect);
	f.render_widget(
		Paragraph::new(lines).style(DIALOG),
		inner.inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 }),
	);
}

fn human(bytes: u64) -> String {
	const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
	let mut v = bytes as f64;
	let mut u = 0;
	while v >= 1024.0 && u < UNITS.len() - 1 {
		v /= 1024.0;
		u += 1;
	}
	if u == 0 {
		format!("{bytes}")
	} else if v < 10.0 {
		format!("{v:.1}{}", UNITS[u])
	} else {
		format!("{v:.0}{}", UNITS[u])
	}
}

fn width_of(s: &str) -> usize {
	unicode_width::UnicodeWidthStr::width(s)
}

fn char_width(c: char) -> usize {
	unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Pad to `width` display columns (wide CJK characters count twice, combining marks not at all).
fn pad(s: &str, width: usize) -> String {
	let len = width_of(s);
	if len >= width { s.to_owned() } else { format!("{s}{}", " ".repeat(width - len)) }
}

/// The longest prefix of `s` that fits in `width` columns.
fn take_width(s: &str, width: usize) -> &str {
	let mut used = 0;
	for (i, c) in s.char_indices() {
		used += char_width(c);
		if used > width {
			return &s[..i];
		}
	}
	s
}

/// The longest suffix of `s` that fits in `width` columns.
fn take_width_end(s: &str, width: usize) -> &str {
	let mut used = 0;
	for (i, c) in s.char_indices().rev() {
		used += char_width(c);
		if used > width {
			return &s[i + c.len_utf8()..];
		}
	}
	s
}

fn truncate_right(s: &str, width: usize) -> String {
	if width_of(s) <= width {
		s.to_owned()
	} else if width == 0 {
		String::new()
	} else {
		format!("{}…", take_width(s, width - 1))
	}
}

fn truncate_left(s: &str, width: usize) -> String {
	if width_of(s) <= width {
		s.to_owned()
	} else if width == 0 {
		String::new()
	} else {
		format!("…{}", take_width_end(s, width - 1))
	}
}

/// Shorten in the middle, keeping the end (the file name) whole when possible.
fn truncate_middle(s: &str, width: usize) -> String {
	if width_of(s) <= width {
		return s.to_owned();
	}
	if width < 5 {
		return truncate_left(s, width);
	}
	let tail = (width * 2 / 3).max(width.saturating_sub(12));
	let head = width - 1 - tail;
	format!("{}…{}", take_width(s, head), take_width_end(s, tail))
}

#[cfg(test)]
pub(crate) mod tests_support {
	pub(crate) fn pad_for_test(s: &str, w: usize) -> String {
		super::pad(s, w)
	}
	pub(crate) fn width_for_test(s: &str) -> usize {
		super::width_of(s)
	}
}
