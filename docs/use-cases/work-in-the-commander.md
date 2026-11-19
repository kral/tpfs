# Work in the commander

**Goal:** tidy files with the keyboard instead of writing JSON: copy photos to a backup, move a
file, rename one, trash one, and undo a mistake with Ctrl+Z.

Every action below builds a plan, shows it, and runs it through the journal, exactly like
`tpfs apply`. The keys and dialogs are described in full in [Commander (TUI)](../tui.md). The
commander needs an interactive terminal, so this page describes the screens rather than showing
captured output.

## Open two folders

```sh
tpfs ~/photos ~/backup
```

The left panel shows `~/photos` and is active; the right panel shows `~/backup`. Tab switches the
active panel. F5 and F6 always copy or move from the active panel to the folder of the other one.

## Copy several photos to the backup (F5)

1. In the left panel, move to the first photo with ↑ ↓, or type the start of its name (quick search).
2. Press Space or Ins to mark it. The cursor moves down; mark the next ones the same way. The panel
   footer shows how many entries are marked and their size.
3. Press F5.

The confirmation dialog opens with the reason `Copy to /home/you/backup`, one `COPY` line (and its
`→` target line) per photo, the totals, the digest and `Risk: low`. Nothing is overwritten: if a
photo with the same name already exists in the backup, the dialog lists the problem and offers only
`[ Esc: Close ]`.

A low-risk copy that removes nothing proceeds on **Enter**. The status line then says
`Copy to /home/you/backup: done, 3 step(s). Ctrl+Z to undo.`, the marks are cleared, and both panels
reload.

## Move a file (F6)

Put the cursor on a file without marking anything, and press F6. With nothing marked, F6 acts on the
entry under the cursor. The dialog shows one `MOVE` line; Enter proceeds.

## Rename a file (F2)

Put the cursor on `IMG_0412.jpg` and press F2. An input box opens with the current name. Edit it to
`beach.jpg` and press Enter.

A valid, low-risk rename runs right away, without a confirmation dialog, because you just typed
exactly what should happen. It is still journaled, so Ctrl+Z undoes it. If the new name already
exists, the plan is shown with the problem instead, and nothing changes.

Making a folder with F7 works the same way: type the name, press Enter.

## Trash a file (F8 or Del)

Put the cursor on `setup.zip` and press F8. The dialog shows a red `TRASH` line and the note
`Nothing is overwritten; trashed items stay in the trash; Ctrl+Z undoes it.`

Because the plan removes something, **Enter does nothing here**; the key line says
`[ Y: Proceed ]   [ Esc: Cancel ]`. Press `y` to trash the file, or Esc to cancel. This way a stray
Enter can never trash anything.

If you mark a folder and press F8, the plan is HIGH risk ("moves 1 whole folder(s) to the trash"),
the dialog frame turns red, and `y` works only after you have scrolled through every line with ↓ or
PgDn.

## Undo a mistake (Ctrl+Z or F9)

Press Ctrl+Z. The Undo dialog shows the newest batch in the journal before it does anything. Its
layout, from the code:

```text
Batch <batch id> by a person
Move to trash

These changes will be reversed:
TRASH   /home/you/photos/setup.zip

[ Y: Undo ]   [ Esc: Cancel ]
```

The newest batch may not be yours: an AI agent or another terminal may have changed something since.
The dialog says `by an AI agent` in that case. Press `y` to undo it. The status line reports
`Undone batch …: 1 step(s).` and the file is back. Press Ctrl+Z again to undo the batch before
that.

If another program wrote a new batch while the dialog was open, nothing is undone, and the status
line asks you to press Undo again.

## Check the result from the command line

The commander writes to the same journal as the CLI, so `tpfs log` in another terminal lists your
commander batches with the source `human`, and `tpfs undo` works on them too.

## Tips

- Esc cancels any dialog: `Cancelled; nothing was changed.` A mouse click on a dialog cancels it
  too; a click never confirms.
- Shift+F2 compares the two panels and builds a sync plan; see
  [Compare and sync](../tui.md#compare-and-sync).
- `:` opens a command line for a shell command in the current folder. Those commands are not
  journaled and cannot be undone with Ctrl+Z.
