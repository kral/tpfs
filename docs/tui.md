# Commander (TUI)

`tpfs` includes a minimal two-panel file manager in the style of Norton Commander. It has no file
operations of its own: Copy, Move, Rename, Make directory and Trash build a [plan](concepts.md#plans-and-operations),
show it with its risk, and execute it through the journal. Every change can be undone with Ctrl+Z.

## Start it

```sh
tpfs                         # both panels in the current folder
tpfs ~/photos                # left panel in ~/photos, right panel in the current folder
tpfs ~/photos ~/backup       # one folder per panel
tpfs ui --icons              # with Nerd Font icons (or set TPFS_ICONS=1)
```

The commander needs an interactive terminal. It uses the same [state folder](concepts.md#the-state-folder)
as the other commands, so `--home` and `TPFS_HOME` apply, and `tpfs log` and `tpfs undo` in another
terminal see what you did here.

You may work anywhere on your disks: the commander uses every drive (or `/`) as its roots. Paths
through symlinks are still refused, and tpfs's state folder stays protected.

## The screen

Each panel shows its folder in the title. The list starts with `..` (its size column says `<UP>`),
then the folders (with a trailing `/` and `<DIR>` in the size column), then the files with their
size and date, each group sorted by name. The active panel is the one the
keys act on; Tab switches. Marked entries are highlighted, and the panel footer shows how many are
marked and their total size. The bottom line is the status line: it shows the result of the last
action. Below it is the F-key bar: 1 Help, 2 Rename, 3 View, 4 Edit, 5 Copy, 6 Move, 7 Mkdir,
8 Trash, 9 Undo, 10 Quit.

## Keys

F1 shows a short version of this list.

### Moving around

| Key | Action |
|---|---|
| ↑ ↓, PgUp PgDn, Home End | move the cursor |
| Enter, → | enter the folder under the cursor (Enter on a file opens the viewer) |
| Backspace, ← | go to the parent folder; at the top of a drive or mount, open the drive list |
| Tab, Shift+Tab | switch panels |
| Alt+F1, Alt+F2 | pick a drive (Windows) or mount point for the left or right panel |
| letters | quick search: jump to the first name that starts with what you type |
| Ctrl+R | reload both panels |

Quick search ignores letter case and common accents, so `zal` finds `zálohy`. It starts over after
about a second without typing; Backspace removes a letter, Esc ends it.

The drive list shows the drives on Windows. On Linux and macOS it shows `/`, your home folder, and
the folders in `/mnt`, `/media/$USER` and `/run/media/$USER`. On Windows, type a drive letter to jump
to it.

### Marking

| Key | Action |
|---|---|
| Ins, Space, right click | mark or unmark the entry under the cursor and move down |

Copy, Move and Trash act on the marked entries. When nothing is marked, they act on the entry under
the cursor.

### Changing files

| Key | Action | Plan |
|---|---|---|
| F5 | copy to the other panel's folder | one `copy` per entry |
| F6 | move to the other panel's folder | one `move` per entry |
| F2 | rename the entry under the cursor | `rename` |
| F7 | make a folder here | `mkdir` |
| F8, Del | move to the trash | one `trash` per entry |
| Shift+F2 | compare the two panels' folders and sync them | see [Compare and sync](#compare-and-sync) |
| F9, Ctrl+Z | undo the last change | see [Undo](#undo) |

F2 and F7 open a small input box (Enter: OK, Esc: Cancel). A folder name is trimmed and may not
contain `/` or `\`. A new file name is taken exactly as typed. If a rename or a new folder is valid
and low-risk, it runs right away; otherwise its plan is shown first. A name with characters the
input box cannot show (control or direction characters, invalid Unicode) cannot be renamed here;
the status line tells you to use `tpfs apply`.

### Other keys

| Key | Action |
|---|---|
| F3 | view the file under the cursor (text, or a hex dump for binary files; first 256 KiB) |
| F4 | edit the file in `$VISUAL` or `$EDITOR`; without them the first of `nano`, `vim`, `vi` found (Notepad on Windows) |
| `:` | command line: runs a shell command in the active panel's folder |
| F10, Ctrl+Q, Ctrl+C | quit |

Shell commands and editor changes are your own: they do not go through tpfs and are not journaled,
so Ctrl+Z cannot undo them. After the command ends, press Enter to return.

In the viewer, ↑ ↓ PgUp PgDn Space Home End scroll, and Esc, F3, F10 or `q` close it.

### Mouse

Click to select an entry and activate its panel, double-click to enter a folder or view a file,
right-click to mark, use the wheel to scroll, and click the F-key bar to press a key. While a
dialog is open, a click closes it and cancels; a click never confirms anything.

## The confirmation dialog

Every plan opens a dialog with the reason, the list of operations, the totals with sizes, the digest
(shortened in the middle, so you can compare both ends with the CLI), the risk, and the keys.
`TRASH` and `RMDIR` lines are red, and a HIGH-risk plan has a red frame.

What confirms depends on the [risk](concepts.md#risk):

| Plan | Key line | To proceed |
|---|---|---|
| low risk, removes nothing (a routine copy, move or mkdir) | `[ Enter: Proceed ]   [ Esc: Cancel ]` | Enter |
| low risk, trashes or removes a folder | `[ Y: Proceed ]   [ Esc: Cancel ]` | `y` |
| MEDIUM | `[ Y: Proceed ]   [ Esc: Cancel ]` | `y` |
| HIGH | `↓ PgDn: read the whole list to proceed   [ Esc: Cancel ]` | scroll to the end, then `y` |

So a stray Enter never trashes anything, and you cannot approve a HIGH-risk plan without having seen
every line of it. Scroll the list with ↑ ↓ PgUp PgDn Space Home End.

If the plan has problems, they are listed in red with `✗`, and the only key is `[ Esc: Close ]`.
If the terminal is too small to show the dialog, nothing can be approved:
`Terminal too small to review this plan — enlarge it.` Esc always cancels:
`Cancelled; nothing was changed.`

After the plan runs, the status line reports it, for example
`Move to /home/you/backup: done, 2 step(s). Ctrl+Z to undo.` Marks are cleared and both panels
reload.

## Undo

F9 or Ctrl+Z opens the Undo dialog for the newest batch in the journal. That batch may come from an
AI agent or from another terminal, so the dialog says who made it (`a person` or `an AI agent`), the
reason, and up to 12 of its steps. Press `y` to undo, Esc to cancel.

If the history changed while the dialog was open, nothing happens:
`the history changed meanwhile; press Undo again to see what it would revert`.

If the newest batch was interrupted or is damaged, the commander does not undo. The status line
tells you what to run in a terminal (`tpfs recover`, or `tpfs log` and `tpfs recover --abandon`).
See [Recover and abandon](concepts.md#recover-and-abandon).

## Compare and sync

Shift+F2 compares the folders of the two panels (some terminals send F14 for Shift+F2; that works
too). The folders must be separate. The dialog lists every difference with the action planned for it,
and the number of identical files.

| Key | Action |
|---|---|
| → | sync left to right |
| ← | sync right to left |
| `b` | both ways (the newer file wins) |
| `m` | toggle mirror: items only in the target go to the trash (one-way only) |
| ↑ ↓ PgUp PgDn | scroll |
| Enter | build the plan and open the confirmation dialog |
| Esc | cancel |

The default direction is from the active panel to the other one. The rules are those of
[`tpfs sync`](cli.md#tpfs-sync), including the
[empty-source guard](use-cases/mirror-a-folder.md#the-empty-source-guard). If nothing is left to do,
the status line says why.

## A worked example

[Work in the commander](use-cases/work-in-the-commander.md) walks through copy, move, rename,
trash and undo.
