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
			let (program, args) = split_command(&editor);
			let program = program.as_str();
			let status = Command::new(program).args(&args).arg(&path).status().map_err(|e| {
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
			let mut run = Command::new(&shell);
			if flag == "/C" {
				// cmd.exe does not read the quoting Rust applies to arguments, and cannot start in a
				// network folder: hand it the line as typed, after a `pushd` there when needed.
				let (in_dir, line) = cmd_line(&cwd.to_string_lossy(), &command);
				if in_dir {
					run.current_dir(&cwd);
				}
				#[cfg(windows)]
				std::os::windows::process::CommandExt::raw_arg(&mut run, &line);
				#[cfg(not(windows))]
				run.arg(&line);
			} else {
				run.arg(flag).arg(&command).current_dir(&cwd);
			}
			let status = run.status().map_err(|e| format!("{shell}: {e}"))?;
			print!("\n[{status}] Press Enter to return to tpfs… ");
			io::stdout().flush().ok();
			io::stdin().lock().read_line(&mut String::new()).ok();
			Ok(format!("Shell command finished ({status}); shell commands are not journaled."))
		}
	}
}

/// The arguments for `cmd.exe` to run `command` in `cwd`, as typed: `/S /C "<line>"`, so cmd
/// strips exactly the outer quotes it was given. A network folder (`\\server\share`) cannot be
/// cmd's starting folder (it would fall back to C:\Windows), so the line starts with `pushd`
/// there, which maps a drive letter for it. `true` when `cwd` can be the starting folder.
fn cmd_line(cwd: &str, command: &str) -> (bool, String) {
	let cwd = match cwd.strip_prefix(r"\\?\UNC\") {
		Some(rest) => format!(r"\\{rest}"),
		None => cwd.strip_prefix(r"\\?\").unwrap_or(cwd).to_owned(),
	};
	if cwd.starts_with(r"\\") {
		(false, format!(r#"/S /C "pushd "{cwd}" && {command}""#))
	} else {
		(true, format!(r#"/S /C "{command}""#))
	}
}

/// The editor for F4: $VISUAL or $EDITOR, else the first common one on PATH (Notepad on Windows).
fn editor() -> Option<String> {
	editor_from(std::env::var("VISUAL").ok(), std::env::var("EDITOR").ok(), std::env::var_os("PATH"), cfg!(windows))
}

fn editor_from(
	visual: Option<String>,
	editor: Option<String>,
	path: Option<std::ffi::OsString>,
	windows: bool,
) -> Option<String> {
	if let Some(e) = [visual, editor].into_iter().flatten().find(|e| !e.trim().is_empty()) {
		return Some(e);
	}
	if windows {
		return Some("notepad".into());
	}
	let path = path?;
	["nano", "vim", "vi"]
		.iter()
		.find(|c| std::env::split_paths(&path).any(|d| d.join(c).is_file()))
		.map(|c| (*c).to_owned())
}

/// A command line as a program and its arguments: a quoted program (`"C:\Program Files\…\x.exe"
/// -n`), a whole value that is an existing file (a path with spaces), or words.
fn split_command(line: &str) -> (String, Vec<String>) {
	let line = line.trim();
	if let Some(rest) = line.strip_prefix('"')
		&& let Some((program, args)) = rest.split_once('"')
	{
		return (program.to_owned(), args.split_whitespace().map(str::to_owned).collect());
	}
	if std::path::Path::new(line).is_file() {
		return (line.to_owned(), Vec::new());
	}
	let mut words = line.split_whitespace().map(str::to_owned);
	(words.next().unwrap_or_default(), words.collect())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_editor_is_chosen_from_the_environment_then_path() {
		let tmp = tempfile::TempDir::new().unwrap();
		let path = || Some(tmp.path().as_os_str().to_owned());
		let s = |v: &str| Some(v.to_owned());
		assert_eq!(editor_from(s("code --wait"), s("vim"), path(), false).as_deref(), Some("code --wait"));
		assert_eq!(editor_from(s("  "), s("vim"), path(), false).as_deref(), Some("vim"));
		assert_eq!(editor_from(None, s(""), path(), false), None, "nothing on PATH");
		std::fs::write(tmp.path().join("vi"), "").unwrap();
		assert_eq!(editor_from(None, None, path(), false).as_deref(), Some("vi"));
		std::fs::write(tmp.path().join("nano"), "").unwrap();
		assert_eq!(editor_from(None, None, path(), false).as_deref(), Some("nano"));
		assert_eq!(editor_from(None, None, None, true).as_deref(), Some("notepad"));
	}

	#[test]
	fn cmd_runs_the_line_as_typed_and_in_network_folders() {
		assert_eq!(
			cmd_line(r"C:\Users\me", r#""C:\Program Files\x.exe" "a b""#),
			(true, r#"/S /C ""C:\Program Files\x.exe" "a b"""#.to_owned())
		);
		let share = (false, r#"/S /C "pushd "\\nas\photos\2024" && del /q *.tmp""#.to_owned());
		assert_eq!(cmd_line(r"\\nas\photos\2024", "del /q *.tmp"), share);
		assert_eq!(cmd_line(r"\\?\UNC\nas\photos\2024", "del /q *.tmp"), share);
		assert_eq!(cmd_line(r"\\?\D:\x", "dir").1, r#"/S /C "dir""#);
	}

	#[test]
	fn editor_commands_keep_paths_with_spaces() {
		let w = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
		assert_eq!(split_command("code --wait"), ("code".into(), w(&["--wait"])));
		assert_eq!(
			split_command(r#""C:\Program Files\Notepad++\notepad++.exe" -multiInst"#),
			(r"C:\Program Files\Notepad++\notepad++.exe".into(), w(&["-multiInst"]))
		);
		let tmp = tempfile::TempDir::new().unwrap();
		let spaced = tmp.path().join("my editor");
		std::fs::write(&spaced, "").unwrap();
		assert_eq!(split_command(spaced.to_str().unwrap()), (spaced.to_str().unwrap().into(), vec![]));
	}
}
