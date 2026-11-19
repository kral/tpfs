# Clean up safely

**Goal:** remove two old installers and an empty folder from `~/Downloads`, then change your mind
and get them back.

tpfs has no delete. `trash` moves an item into tpfs's trash and records where it went, so undo can
put it back. `rmdir` removes only an empty folder.

## Starting point

```console
$ ls -A ~/Downloads
invoice-2026-03.pdf
old-installers
setup-old.zip
setup.zip
```

`old-installers` is empty.

## The plan

Save as `cleanup.json`:

```json
{
  "reason": "Clean up old downloads",
  "ops": [
    { "op": "trash", "path": "/home/you/Downloads/setup.zip" },
    { "op": "trash", "path": "/home/you/Downloads/setup-old.zip" },
    { "op": "rmdir", "path": "/home/you/Downloads/old-installers" }
  ]
}
```

## Preview

```console
$ tpfs check cleanup.json --root ~/Downloads
Plan: Clean up old downloads
  TRASH   /home/you/Downloads/setup.zip
  TRASH   /home/you/Downloads/setup-old.zip
  RMDIR   /home/you/Downloads/old-installers
Total: 3 operation(s): 2 trash (9 B), 1 rmdir
2 item(s) go to the trash (nothing is deleted; `tpfs undo` puts them back).
Risk: low
Digest: ea61bf9c5b3fa17f4a4597c181d3b80c0e5e81c345a4c0f92631a62d0eee8eb2
OK — the plan is valid.
```

On a terminal, the `TRASH` and `RMDIR` lines are red.

## Execute

```console
$ tpfs apply cleanup.json --root ~/Downloads
…
Apply? [y/N] y
Done: 3 step(s), batch 18e97ddfa3b51430-6c6a. Undo with `tpfs undo`.
$ ls -A ~/Downloads
invoice-2026-03.pdf
```

## Where the files went

The trashed files are in the trash of the [state folder](../concepts.md#the-state-folder), under the
batch id and the step number:

```console
$ find ~/.local/state/tpfs/trash
/home/you/.local/state/tpfs/trash
/home/you/.local/state/tpfs/trash/18e97ddfa3b51430-6c6a
/home/you/.local/state/tpfs/trash/18e97ddfa3b51430-6c6a/0
/home/you/.local/state/tpfs/trash/18e97ddfa3b51430-6c6a/0/setup.zip
/home/you/.local/state/tpfs/trash/18e97ddfa3b51430-6c6a/1
/home/you/.local/state/tpfs/trash/18e97ddfa3b51430-6c6a/1/setup-old.zip
```

Files on another disk go to a trash on that disk instead; see [The trash](../concepts.md#the-trash).
The journal records each location as the undo of its step:

```console
$ tpfs log 18e97ddfa3b51430-6c6a
State: /home/you/.local/state/tpfs
Batch 18e97ddfa3b51430-6c6a by human: Clean up old downloads
  #1    done     TRASH   /home/you/Downloads/setup.zip
         undo:    MOVE    /home/you/.local/state/tpfs/trash/18e97ddfa3b51430-6c6a/0/setup.zip  ->  /home/you/Downloads/setup.zip
  #2    done     TRASH   /home/you/Downloads/setup-old.zip
         undo:    MOVE    /home/you/.local/state/tpfs/trash/18e97ddfa3b51430-6c6a/1/setup-old.zip  ->  /home/you/Downloads/setup-old.zip
  #3    done     RMDIR   /home/you/Downloads/old-installers
         undo:    MKDIR   /home/you/Downloads/old-installers
```

## Restore with undo

```console
$ tpfs undo
Undo batch 18e97ddfa3b51430-6c6a by human — 3 step(s): Clean up old downloads
These changes will be reversed:
  TRASH   /home/you/Downloads/setup.zip
  TRASH   /home/you/Downloads/setup-old.zip
  RMDIR   /home/you/Downloads/old-installers
Undo it? [y/N] y
Undone batch 18e97ddfa3b51430-6c6a (3 step(s)):
  reverted: TRASH   /home/you/Downloads/setup.zip
  reverted: TRASH   /home/you/Downloads/setup-old.zip
  reverted: RMDIR   /home/you/Downloads/old-installers
$ ls -A ~/Downloads
invoice-2026-03.pdf
old-installers
setup-old.zip
setup.zip
```

Everything is back, including the empty folder.

## Trashing a whole folder

Trashing a folder is rated HIGH, because a whole tree disappears from view at once. You must type
the full word `yes`:

```console
$ tpfs apply folder.json --root ~/Downloads
Plan: Remove the old folder
  TRASH   /home/you/Downloads/old
Total: 1 operation(s): 1 trash (2 B)
1 item(s) go to the trash (nothing is deleted; `tpfs undo` puts them back).
Risk: HIGH — moves 1 whole folder(s) to the trash
Digest: ad513d92672087d7a3f3cdde06fef64af3a6e5a53883f082f6fcfc7fa1dc085e
Type 'yes' to apply this HIGH-risk plan: y
tpfs apply: cancelled; nothing was changed
```

Answering `yes` applies it, and `tpfs undo` brings the folder back with all its content.

## What tpfs refuses

```console
$ tpfs check bad.json --root ~/Downloads
  RMDIR   /home/you/Downloads
  TRASH   /home/you/Downloads/missing.txt
…
Problems:
  #1: Cannot remove an allowed root folder.
  #2: Does not exist: /home/you/Downloads/missing.txt
tpfs check: plan rejected (see the problems above); nothing was changed
```

`rmdir` of a folder that still has entries fails with `Directory is not empty`. Trash the folder
instead, or trash its content first in the same plan.

## The trash is never emptied

tpfs never deletes anything from its trash, so it grows over time. If you remove a batch's folder
from the trash by hand, undo of that batch can no longer put those items back.
