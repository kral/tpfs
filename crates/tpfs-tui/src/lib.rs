//! # tpfs-tui
//!
//! A deliberately minimal two-panel file manager in the style of Norton Commander.
//! It owns no file operations of its own: F5/F6/F7/F8/F2 build a [`tpfs_core::Plan`], show it with
//! its risk, and only then execute it through the journal — so every change can be undone (Ctrl+Z).

mod app;
mod extra;
mod panel;
mod ui;

use std::{
	io::{self, BufRead, Write},
	path::PathBuf,
	process::Command,
};

pub use app::App;
use app::External;
use ratatui::crossterm::{
	event::{self, DisableMouseCapture, EnableMouseCapture, Event},
	execute,
};

#[derive(Clone, Debug, Default)]
pub struct Options {
	/// Show Nerd Font icons (needs a Nerd Font in the terminal).
	pub icons: bool,
}

/// Run the commander in the current terminal until the user quits.
pub fn run(left: PathBuf, right: PathBuf, state: PathBuf, options: Options) -> io::Result<()> {
	use std::io::IsTerminal;
	if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
		return Err(io::Error::other("the commander needs an interactive terminal"));
	}
	let mut app = App::new(left, right, state, options.icons)?;

	let mut terminal = ratatui::init();
	// ratatui's hook leaves raw mode and the alternate screen; mouse reporting must go too, or the
	// shell receives escape garbage for every mouse move after a panic.
	let restore = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		let _ = execute!(io::stdout(), DisableMouseCapture);
		restore(info);
	}));
	// Mouse support is a convenience: a terminal that refuses it still gets the commander.
	let _ = execute!(io::stdout(), EnableMouseCapture);
	let result = (|| -> io::Result<()> {
		while !app.quit {
			terminal.draw(|f| ui::draw(f, &mut app))?;
			match event::read()? {
				Event::Key(key) => app.on_key(key),
				Event::Mouse(mouse) => app.on_mouse(mouse),
				_ => {}
			}

			if let Some(external) = app.external.take() {
				let _ = execute!(io::stdout(), DisableMouseCapture);
				ratatui::restore();
				let outcome = run_external(external);
				terminal = ratatui::init();
				let _ = execute!(io::stdout(), EnableMouseCapture);
				terminal.clear()?;
				app.after_external(outcome);
			}
		}
		Ok(())
	})();
	// Always give the terminal back, whatever failed above.
	let _ = execute!(io::stdout(), DisableMouseCapture);
	ratatui::restore();
	result
}

/// Run an editor or a shell command on the real terminal. Shell commands are the user's own:
/// they do not go through tpfs and are not journaled.
fn run_external(external: External) -> Result<String, String> {
	match external {
		External::Edit(path) => {
			let editor = editor().ok_or(
				"no editor found: set the EDITOR environment variable to the editor you use (e.g. nano, vim, code --wait)",
			)?;
			let mut parts = editor.split_whitespace();
			let program = parts.next().unwrap_or_default();
			let status = Command::new(program).args(parts).arg(&path).status().map_err(|e| {
				if e.kind() == io::ErrorKind::NotFound {
					format!("editor `{program}` not found: set EDITOR to the editor you use")
				} else {
					format!("{program}: {e}")
				}
			})?;
			Ok(format!("{program} exited ({status})."))
		}
		External::Shell { command, cwd } => {
			println!("{}> {}", tpfs_core::sanitize(cwd.as_os_str()), tpfs_core::sanitize(command.as_ref()));
			// The user's shell: $SHELL where set (also Git Bash on Windows), else cmd.exe on Windows.
			let (shell, flag) = match std::env::var("SHELL").ok().filter(|s| !s.is_empty()) {
				Some(sh) => (sh, "-c"),
				None if cfg!(windows) => (std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into()), "/C"),
				None => ("sh".into(), "-c"),
			};
			let status = Command::new(&shell)
				.arg(flag)
				.arg(&command)
				.current_dir(&cwd)
				.status()
				.map_err(|e| format!("{shell}: {e}"))?;
			print!("\n[{status}] Press Enter to return to tpfs… ");
			io::stdout().flush().ok();
			io::stdin().lock().read_line(&mut String::new()).ok();
			Ok(format!("Shell command finished ({status}); shell commands are not journaled."))
		}
	}
}

/// The editor for F4: $VISUAL or $EDITOR, else the first common one on PATH (Notepad on Windows).
fn editor() -> Option<String> {
	if let Some(e) = ["VISUAL", "EDITOR"].iter().find_map(|v| std::env::var(v).ok().filter(|e| !e.trim().is_empty())) {
		return Some(e);
	}
	let candidates: &[&str] = if cfg!(windows) { &["notepad"] } else { &["nano", "vim", "vi"] };
	if cfg!(windows) {
		return candidates.first().map(|c| (*c).to_owned());
	}
	let path = std::env::var_os("PATH")?;
	candidates.iter().find(|c| std::env::split_paths(&path).any(|d| d.join(c).is_file())).map(|c| (*c).to_owned())
}
