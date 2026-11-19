# Concepts

This page explains how tpfs works and why. Commands and flags are listed in [Command line](cli.md);
the JSON format is in [Plan format](plan-format.md).

- [Plans and operations](#plans-and-operations)
- [Validation](#validation)
- [Risk](#risk)
- [Digest](#digest)
- [Approval](#approval)
- [The journal](#the-journal)
- [Undo](#undo)
- [Recover and abandon](#recover-and-abandon)
- [The trash](#the-trash)
- [Roots and confinement](#roots-and-confinement)
- [The state folder](#the-state-folder)

## Plans and operations

Every change goes through a **plan**: an ordered list of operations and a short reason. There are
six operations:

| Operation | What it does | How undo reverts it |
|---|---|---|
| `mkdir` | creates a folder | removes the folder (`rmdir`) |
| `rmdir` | removes an **empty** folder | creates it again, with its permissions |
| `move` | moves a file or folder to a new full path | moves it back |
| `copy` | copies a file or folder (recursively) | moves the copy to the trash |
| `rename` | gives an item a new name in the same folder | renames it back |
| `trash` | moves an item into tpfs's [trash](#the-trash) | moves it back from the trash |

There is no delete and no overwrite. A plan that wants to replace a file trashes the old one first
and then puts the new one in its place, so the old version can be restored.

The commander builds plans from your key presses, `tpfs sync` builds them from a folder comparison,
and an agent sends them over MCP. They all go through the same steps below.

## Validation

Before anything touches the disk, tpfs checks the whole plan. It simulates the steps in order, so
"create folder X, then move a file into X" is valid, while "move the same file twice" is not.
A plan is rejected if any step:

- uses a relative path, or a path outside the [allowed roots](#roots-and-confinement);
- goes through a symlink below a root;
- names a source that does not exist, or a target that already exists;
- moves or copies into a folder that does not exist (yet);
- puts a folder inside itself;
- moves, removes or trashes an allowed root itself;
- removes a folder that is not empty;
- renames to a name that contains `/` or `\`, or is `.` or `..`;
- touches tpfs's [state folder](#the-state-folder) or one of tpfs's own work files
  (`.tpfs-trash-<uid>`, `.tpfs-part-…`, `.tpfs-case-…`);
- moves, trashes or removes a mount point (another disk, a share, a subvolume): work on what is
  inside it;
- copies a FIFO, socket or device file (reading one can block forever or read a whole disk);
- copies more data than the target disk has free, counting everything the plan writes there;
- uses a name that is not valid Unicode (not supported yet).

On Windows, tpfs also refuses names Windows cannot use safely: device names such as `CON` or
`com1.txt`, names that end in a dot or a space, and the characters `: < > " | ? *`.

Limits: 10 000 operations per plan (raise it for your own plans with `--max-ops`), 10 000
characters of reason, 4096 bytes and 256 folders deep per path, 64 MiB per plan file.

Each problem is listed with the number of its step, and nothing is changed:

```console
$ tpfs check bad.json --root ~/Downloads
…
Problems:
  #1: Target already exists: /home/you/Downloads/Photos ('to' is the full new path: to put it inside that folder, use /home/you/Downloads/Photos/IMG_0412.jpg)
  #2: Source does not exist: /home/you/Downloads/nothere.jpg
  #3: Outside the allowed folders: /etc/hosts
  #4: Path must be absolute: Downloads/setup.zip
tpfs check: plan rejected (see the problems above); nothing was changed
```

Paths are normalised before they are checked and shown: `/a/tmp/../b` is checked, shown and
executed as `/a/b`, so the preview cannot read differently from what happens. Names with control
characters or invisible Unicode are shown escaped (for example `\u{202e}`), so a file name cannot
disguise itself in the preview.

During execution, tpfs checks each step again right before it runs, because another program may have
changed the disk since the preview.

## Risk

tpfs rates every plan as **low**, **MEDIUM** or **HIGH**. The engine computes the rating; whoever
proposed the plan cannot set it.

| Rated | When |
|---|---|
| HIGH | the plan comes from an agent |
| HIGH | more than 200 operations |
| HIGH | it trashes at least one whole folder |
| MEDIUM | more than 20 operations |
| MEDIUM | it trashes more than 10 files |
| MEDIUM | it trashes a file and then writes a new one at the same path (a replacement) |
| MEDIUM | it trashes a file that has other hard links (only one name goes; the data stays) |
| MEDIUM | a move crosses file systems (the data is copied, and undo copies it back) |
| low | anything else |

The rating decides how carefully you must confirm (see [Approval](#approval)). The preview lists the
reasons:

```text
Risk: HIGH — moves 1 whole folder(s) to the trash
```

## Digest

The digest is the SHA-256 of the plan's exact JSON form. Any change to the plan, even one path or
the reason, gives a different digest. An approval always names a digest, so what you approve is
exactly what runs.

## Approval

Nothing runs without an approval bound to the plan:

- **On a terminal**, `tpfs apply` and `tpfs sync --apply` ask `Apply? [y/N]`. For a HIGH-risk plan
  you must type the full word `yes`.
- **In a script**, pass the digest you reviewed: `--approve <digest>`. If the plan changed, its digest
  no longer matches and nothing happens (exit code 4). There is deliberately no `--yes` flag.
- **Plans from agents** (`--source agent`, or anything proposed over MCP) always need an explicit
  approval: on the command line `--approve <digest>`, over MCP `tpfs approve <planId>`, which only
  works interactively (unless the server runs with `--approval client`). See [MCP server](mcp.md).
- **In the commander**, a low-risk plan that removes nothing runs on Enter. Anything that trashes or
  removes a folder, or has a higher risk, needs `y`. For a HIGH-risk plan, `y` works only after every
  line of the plan has been on screen. See [Commander](tui.md#the-confirmation-dialog).

## The journal

Each run of a plan is a **batch** with an id such as `18e97ddd3e09330c-6c01`. tpfs writes every
batch to a write-ahead journal, `journal.jsonl` in the [state folder](#the-state-folder):

1. a `begin` record with the reason, the source (human or agent) and the digest;
2. for each step, an `intent` record with the operation **and its inverse**, flushed to disk before
   the step touches anything;
3. a `done` record after the step;
4. `commit` at the end, or `abort` if the batch was rolled back.

Because the inverse is on disk before each step runs, tpfs always knows how to put things back,
even after a crash in the middle of a step. Each record carries a checksum, so a damaged line is
detected instead of misread.

Only one tpfs process writes to a journal at a time; a second one gets
`journal is in use by another tpfs process`. While `tpfs apply`, `tpfs sync --apply`, `tpfs undo` or
`tpfs recover --abandon` wait for your answer they do not hold the journal, so the commander and the
MCP server keep working. If another batch was committed meanwhile, `tpfs undo` stops with
`the history changed meanwhile` instead of undoing a batch you did not see.

### Archived history

Every undo, recovery and plan reads the whole journal, so finished history does not stay in it
forever. When the journal is 4 MiB or more as tpfs opens it, the batches nothing can act on any more
move to `journal-archive.jsonl` next to it: undone, rolled back and abandoned batches, and committed
ones older than the newest 1000. Those older committed batches can then no longer be undone. Nothing
that started at or after an interrupted or damaged batch is archived, and nothing at all while the
journal has unreadable lines. `tpfs log` says when there is an archive; `tpfs log --all` lists it
too, and `tpfs log <batch>` finds an archived batch.

The move is crash-safe: the shorter journal is written in full under a private name first, the
archived records are added to the archive, and only then does a rename put the new journal in place.

If a step fails while a batch runs, tpfs rolls back the steps before it right away and reports
`step #N failed (…); previous steps were rolled back`. The batch shows as `rolled back` in `tpfs log`.

`tpfs log` lists the batches; `tpfs log <batch>` lists the steps of one batch with their undo:

```console
$ tpfs log 18e97ddfa3b51430-6c6a
State: /home/you/.local/state/tpfs
Batch 18e97ddfa3b51430-6c6a by human: Clean up old downloads
  #1    done     TRASH   /home/you/Downloads/setup.zip
         undo:    MOVE    /home/you/.local/state/tpfs/trash/18e97ddfa3b51430-6c6a/0/setup.zip  ->  /home/you/Downloads/setup.zip
  …
```

A batch has one of these states in `tpfs log`:

| State | Meaning |
|---|---|
| `committed` | ran completely; can be undone |
| `rolled back` | a step failed or it was recovered; the disk is as before |
| `undone` | reverted by undo |
| ``INCOMPLETE (run `tpfs recover`)`` | interrupted by a crash or power loss |
| ``DAMAGED (see `tpfs log <batch>`)`` | some of its journal records are unreadable |
| `abandoned (changes left on disk)` | you gave up on it with `tpfs recover --abandon` |

## Undo

`tpfs undo` reverts the newest committed batch, whoever made it: you, the commander, or an agent.
It shows the batch and asks first. Run it again to go further back, one batch at a time.

Undo runs the inverse of each step, last step first. It is careful with your data:

- If you changed a copy after tpfs made it, undo stops and keeps the copy:
  `… was changed after it was copied; it is kept. Move it away (or trash it yourself) and try again.`
- If undo stops half way, the steps already reverted stay reverted, and the next `tpfs undo`
  continues where it stopped.
- If a newer batch was interrupted, undo refuses until you run `tpfs recover`, because the older
  inverses assume the newer changes are gone.

Undo may touch any folder the journal names, not only the roots of the original plan. Symlinks
below them are still refused, and the state folder stays protected.

Over MCP, an agent can only undo a batch an agent made. See [MCP server](mcp.md#tools).

## Recover and abandon

A crash, a kill or a power loss can stop a batch between two steps. The journal then shows it as
`INCOMPLETE`, and tpfs refuses new plans and undo until you deal with it:

```console
$ tpfs recover
Rolled back 1 interrupted batch(es).
```

`tpfs recover` runs the inverse of every finished step of the interrupted batch, newest first, and
records each one. If it stops on an error, fix the cause and run it again; finished steps are
skipped.

Recovery does not roll a batch back automatically when that would be unsafe:

- **Damaged journal records.** If some records of the batch are unreadable, tpfs cannot be sure
  which steps ran.
- **Newer changes on top.** If newer batches were committed after the interrupted one, its inverses
  would run over those later changes.

In both cases, look at what the batch did with `tpfs log <batch>`, fix the disk by hand if you need
to, and then tell tpfs to move past it:

```sh
tpfs recover --abandon <batch>
```

This records the batch as abandoned and changes nothing on disk. A committed batch with damaged
records cannot be undone automatically either; `--abandon` works for it too.

A walk-through is in [Recover after a crash](use-cases/recover-after-a-crash.md).

## The trash

`trash` moves an item, it does not delete it. The item goes to:

- `<state folder>/trash/<batch>/<step>/<name>` when it lives on the same file system as the state
  folder;
- on Linux and macOS, when it lives on another file system, `.tpfs-trash-<uid>` at the top of that
  file system (for example `/mnt/usb/.tpfs-trash-1000/<batch>/<step>/<name>`). Trashing is then a
  rename instead of a copy. tpfs uses this folder only if it belongs to you and nobody else can read
  it;
- on Windows, always the trash in the state folder: trashing an item on another drive copies it to
  the state folder's drive.

Undo moves the item back from where the journal says it went. tpfs never empties the trash. If you
delete something from it by hand, undo of that batch can no longer put it back.

## Roots and confinement

A plan may only touch paths inside its **roots**: the folders you name with `--root`, the two
folders of `tpfs sync`, or the folders an MCP server was started with. Roots are resolved to their
real path first (symlinks followed), so a root is taken as it really is.

Below a root, tpfs never follows a symlink. A path that goes through one is refused at validation,
and each operation opens its folders from the root without following links (`openat2` on Linux,
`O_NOFOLLOW_ANY` on macOS, handle-based calls on Windows). A link swapped in after the preview makes
the operation fail instead of redirecting it.

If tpfs cannot set up this protection, it refuses to run rather than run unprotected. This happens
on Linux kernels without `openat2` (older than 5.6, or a container that filters it), and on Windows
for a root that cannot be opened safely. Set `TPFS_ALLOW_UNCONFINED=1` to proceed with a weaker
check: tpfs then re-checks each path for links right before the operation, which leaves a small
window.

The commander, `tpfs undo` and `tpfs recover` use every drive (or `/`) as roots, plus on Windows the
network shares involved, because a person may work anywhere. Symlinks below them are still refused,
and the state folder stays protected.

## The state folder

tpfs keeps its own data in one private folder:

| Path | Content |
|---|---|
| `journal.jsonl` | the [journal](#the-journal) |
| `journal.lock` | held by the tpfs process using the journal |
| `journal-archive.jsonl` | [archived history](#archived-history) |
| `trash/` | the [trash](#the-trash) |
| `pending/` | plans proposed by agents (`<id>.json`) and approvals (`<id>.approved`) |

tpfs picks the folder in this order:

1. `--home <dir>`;
2. the `TPFS_HOME` environment variable;
3. on Windows, `%LOCALAPPDATA%\tpfs`;
4. `$XDG_STATE_HOME/tpfs`;
5. `~/.local/state/tpfs`.

`tpfs log` prints the folder it uses on its first line. On Unix the folder is created with
permissions `0700`, and tpfs refuses a state folder that others can use:

```text
tpfs log: refusing to use /home/you/shared: it must be a real folder owned by you that only you can use (chmod 700)
```

No plan can read, change or contain the state folder:

```text
#1: /home/you/.local contains tpfs's state (/home/you/.local/state/tpfs); leave it out of the plan.
#2: This is tpfs's state (journal, trash, approvals) and cannot be part of a plan: /home/you/.local/state/tpfs/journal.jsonl
```

Without this rule, an agent whose root contains the state could forge the journal or approve its
own plan.

Use the same state folder for all your tpfs commands. Undo, recover and approvals only see the
batches and plans of the state folder they run with.
