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
	execute!(io::stdout(), EnableMouseCapture)?;
	let result = (|| -> io::Result<()> {
		while !app.quit {
			terminal.draw(|f| ui::draw(f, &mut app))?;
			match event::read()? {
				Event::Key(key) => app.on_key(key),
				Event::Mouse(mouse) => app.on_mouse(mouse),
				_ => {}
			}

			if let Some(external) = app.external.take() {
				execute!(io::stdout(), DisableMouseCapture)?;
				ratatui::restore();
				let outcome = run_external(external);
				terminal = ratatui::init();
				execute!(io::stdout(), EnableMouseCapture)?;
				terminal.clear()?;
				app.after_external(outcome);
			}
		}
		Ok(())
	})();
	execute!(io::stdout(), DisableMouseCapture)?;
	ratatui::restore();
	result
}

/// Run an editor or a shell command on the real terminal. Shell commands are the user's own:
/// they do not go through tpfs and are not journaled.
fn run_external(external: External) -> Result<String, String> {
	match external {
		External::Edit(path) => {
			let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
			let mut parts = editor.split_whitespace();
			let program = parts.next().unwrap_or("vi");
			let status =
				Command::new(program).args(parts).arg(&path).status().map_err(|e| format!("{program}: {e}"))?;
			Ok(format!("{program} exited ({status})."))
		}
		External::Shell { command, cwd } => {
			let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
			println!("{}> {}", tpfs_core::sanitize(cwd.as_os_str()), tpfs_core::sanitize(command.as_ref()));
			let status = Command::new(&shell)
				.arg("-c")
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
