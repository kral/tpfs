# Command line

This page lists every command and flag of `tpfs` 0.4.0. `tpfs --help` prints the same list in short,
and `tpfs <command> --help` prints the part about one command. For what plans, roots and the journal
are, see [Concepts](concepts.md).

```text
tpfs [ui] [<left dir>] [<right dir>] [--icons]
tpfs schema
tpfs check  <plan.json|-> --root <dir>... [--source human|agent] [--max-ops <n>]
tpfs apply  <plan.json|-> --root <dir>... [--approve <digest>] [--source human|agent] [--max-ops <n>]
tpfs sync <source> <target> [--both] [--mirror] [--apply] [--approve <digest>] [--json]
tpfs undo [--approve <batch>]
tpfs log [--all] [<batch>]
tpfs recover [--abandon <batch>]
tpfs mcp --root <dir>... [--approval terminal|client]
tpfs pending
tpfs approve <planId>
tpfs reject <planId>
tpfs watch
tpfs recipes
tpfs recipe install <id> [--root <dir>...]
tpfs recipe reject <id>
tpfs recipe show <id>
tpfs recipe run <recipe> <entry>... [--set <field>=<value>]... [--approve <digest>]
tpfs recipe remove <recipe>
tpfs guard
tpfs doctor [<project>]
```

Every command also takes `--home <dir>` (see [Global options](#global-options)). A flag a command
does not use is an error, not silently ignored:

```console
$ tpfs schema --root d
tpfs: schema: --root does not apply here (see `tpfs schema --help`)
```

## Exit codes

| Code | Meaning |
|---|---|
| 0 | done, also when there was nothing to do |
| 1 | failed (an I/O error, a blocked or failed batch, recovery errors) |
| 2 | usage error (unknown command or flag, missing argument, unknown batch or plan id) |
| 3 | the plan was rejected by validation; nothing was changed |
| 4 | cancelled or not approved; nothing was changed |

Errors go to stderr as `tpfs <command>: <message>` (or `tpfs: <message>` for a mistake in the
arguments). Previews and results go to stdout.

## Global options

| Option | Effect |
|---|---|
| `--home <dir>` | the state folder (journal, trash, pending plans). See [The state folder](concepts.md#the-state-folder) for the default. |
| `-h`, `--help` | show the help; `tpfs <command> --help` shows one command |
| `-V`, `--version` | print the version |

## tpfs, tpfs ui

```text
tpfs [ui] [<left dir>] [<right dir>] [--icons]
```

Opens the two-panel [commander](tui.md). Without folders, both panels show the current folder. With
one folder, the left panel shows it. When the first argument is a folder, `ui` can be left out:
`tpfs ~/photos ~/backup`.

- `--icons` shows Nerd Font icons; your terminal needs a Nerd Font. `TPFS_ICONS=1` does the same.

`tpfs` without a command opens the commander only when stdin and stdout are a terminal. Otherwise:

```console
$ tpfs </dev/null
tpfs: missing command (see `tpfs --help`)
```

`tpfs ui` outside a terminal fails with `the commander needs an interactive terminal` (exit 1).

## tpfs schema

Prints the JSON Schema of a plan. Use it in prompts or to validate plans in an editor. The format
is described in [Plan format](plan-format.md).

## tpfs check

```text
tpfs check <plan.json|-> --root <dir>... [--source human|agent] [--max-ops <n>]
```

Validates a plan and prints its preview, totals, risk and digest. It never changes anything. Use
`-` to read the plan from stdin.

| Flag | Effect |
|---|---|
| `--root <dir>` | required, repeatable: a folder the plan may touch |
| `--source human\|agent` | rate the risk as for this proposer (default `human`); `agent` makes it HIGH |
| `--max-ops <n>` | allow more than 10 000 operations in a plan you made yourself; refused with `--source agent` |

Output of a valid plan:

```console
$ tpfs check organise.json --root ~/Downloads
Plan: Sort downloads into folders
  MKDIR   /home/you/Downloads/Invoices
  MKDIR   /home/you/Downloads/Photos
  MOVE    /home/you/Downloads/invoice-2026-03.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-03.pdf
  …
Total: 6 operation(s): 4 move (21 B), 2 mkdir
Risk: low
Digest: af624d8ac50c3d4153f2e93a5df7c11206cb5d04458e296f5e450f228bd0263d
OK — the plan is valid.
```

- `Total` counts the operations by kind, with the size of what is moved, copied or trashed.
- A plan that trashes something adds the line
  ``N item(s) go to the trash (nothing is deleted; `tpfs undo` puts them back).``
- On a terminal, `TRASH` and `RMDIR` lines are shown in red. Set `NO_COLOR` to turn this off.

An invalid plan lists `Problems:` and exits with 3; an example is in
[Validation](concepts.md#validation). JSON that is not a plan exits with 3 too:

```console
$ echo '{"ops":[{"op":"delete","path":"/x"}]}' | tpfs check - --root ~/Downloads
tpfs check: not a valid plan: unknown variant `delete`, expected one of `move`, `copy`, `rename`, `trash`, `mkdir`, `rmdir` at line 1 column 22
```

A plan with no operations is not an error:

```console
$ echo '{"ops":[]}' | tpfs check - --root ~/Downloads
Nothing to do: the plan has no operations.
```

## tpfs apply

```text
tpfs apply <plan.json|-> --root <dir>... [--approve <digest>] [--source human|agent] [--max-ops <n>]
```

Does what `check` does, then asks for approval and executes the plan as one batch. It takes the
flags of `check`, plus:

| Flag | Effect |
|---|---|
| `--approve <digest>` | approve exactly the plan with this digest, without a question |

How approval works:

- On a terminal, without `--approve`, it asks `Apply? [y/N]`. A HIGH-risk plan asks
  `Type 'yes' to apply this HIGH-risk plan: ` and accepts only `yes`.
- Without a terminal, or when the plan comes from stdin (`-`), you must pass `--approve`:
  `tpfs apply: not interactive: pass --approve <digest> to apply exactly this plan` (exit 4).
- With `--source agent`, you must pass `--approve`:
  `tpfs apply: plans from agents need a human approval: --approve <digest>` (exit 4).
- A digest that does not match: `--approve does not match this plan's digest; nothing was changed`
  (exit 4).

On success:

```text
Done: 6 step(s), batch 18e97ddd3e09330c-6c01. Undo with `tpfs undo`.
```

If an earlier batch was interrupted, `apply` refuses until you run [`tpfs recover`](#tpfs-recover):

```text
tpfs apply: batch 18e97de4eaba61d8-6e58 was interrupted; run `tpfs recover` first
```

## tpfs sync

```text
tpfs sync <source> <target> [--both] [--mirror] [--apply] [--approve <digest>] [--json]
```

Compares two folders and builds a plan that brings the target up to date. Without `--apply` or
`--approve` it is a dry run. The two folders are the roots of the plan.

| Flag | Effect |
|---|---|
| (none) | one way, source to target: copy what is missing, replace older files |
| `--both` | both ways: copy what is missing on either side; the newer file wins |
| `--mirror` | one way, and move items that exist only in the target to the trash |
| `--apply` | confirm on the terminal and execute (as `tpfs apply`) |
| `--approve <digest>` | execute without a question, if the plan has this digest |
| `--json` | print the plan as JSON instead of the preview; summary on stderr |

How it compares:

- Files are compared by modification time and size. Times less than 2 seconds apart count as
  equal, because FAT and many NAS file systems store times in 2-second steps.
- A newer file replaces an older one: the plan trashes the old file, then copies the new one.
- A folder that exists on one side only is copied as a whole.
- tpfs's own work files and, when deciding whether a source folder is empty, OS metadata such as
  `.DS_Store`, `Thumbs.db` and `desktop.ini` are ignored.
- The comparison stops at 200 000 entries: `too many files to compare (limit 200000)`.

Some differences are never synced, and the preview lists them with the reason:

| Shown as | Why |
|---|---|
| `only on the right; mirror is off` | exists only in the target, and `--mirror` is not set |
| `the right one is newer; would replace a newer file` | one-way sync never replaces a newer file |
| `same time, different size; decide by hand` | nobody can tell which one is right |
| `file on one side, folder or link on the other` | type mismatch |
| `same name in a different letter case; rename one by hand` | on Windows and macOS only |
| `symlink or special file` | links and special files are not synced |
| `the name is not valid Unicode; handle it by hand` | not supported yet |
| `its source folder is empty (is a disk not mounted?); a mirror would trash it — not planned` | the [empty-source guard](use-cases/mirror-a-folder.md#the-empty-source-guard) |

Rules:

- Source and target must be separate: `source and target must be separate folders (one is inside
  the other)` (exit 2).
- `--both` and `--mirror` cannot be combined (exit 2).
- `--json` cannot be combined with `--apply` or `--approve` (exit 2).
- A missing folder is an error (exit 2), never treated as an empty one.
- `sync` does not take `--max-ops`. For a sync of more than 10 000 operations, write the plan with
  `--json` and run it with `tpfs apply --max-ops`.

A full example is in [Mirror a folder](use-cases/mirror-a-folder.md).

## tpfs undo

```text
tpfs undo [--approve <batch>]
```

Reverts the newest committed batch. See [Undo](concepts.md#undo).

- On a terminal it lists the batch and asks `Undo it? [y/N]`.
- Without a terminal you must name the batch with `--approve <batch>`. Otherwise it stops with exit 4:

  ```text
  tpfs undo: not interactive: pass --approve 18e97ddd620dcbd3-6c1a to undo exactly that batch (see `tpfs log`)
  ```

- A batch id that is not the newest one: `--approve nope does not name the newest batch (…); nothing
  was changed` (exit 4).
- If another process committed a newer batch after you were shown one, nothing is undone:
  ``the history changed meanwhile: …; run `tpfs undo` again`` (exit 4).
- With nothing to undo it prints `Nothing to undo.` (exit 0).

On success it lists what it reverted:

```text
Undone batch 18e97ddd620dcbd3-6c1a (6 step(s)):
  reverted: MKDIR   /home/you/Downloads/Invoices
  …
```

## tpfs log

```text
tpfs log [--all] [<batch>]
```

Without an argument, lists the batches in the journal, oldest first: id, source, state, number of
steps and reason. `--all` adds the [archived history](concepts.md#archived-history) before them; when
there is an archive and `--all` is not given, a note on stderr says so.
The first line, on stderr, names the state folder.

```console
$ tpfs log
State: /home/you/.local/state/tpfs
18e97ddd3e09330c-6c01  human  committed  6 step(s)  Sort downloads into folders
```

The states are explained in [The journal](concepts.md#the-journal). If the journal has unreadable
lines, a warning follows: `warning: 1 unreadable journal line(s) skipped`.

With a batch id, lists every step of that batch, its state (`done`, `started`, `failed` or `undone`)
and the operation that undoes it; archived batches are found too. An unknown id is a usage error
(exit 2).

## tpfs recover

```text
tpfs recover [--abandon <batch>]
```

Rolls back batches that a crash interrupted. See [Recover and abandon](concepts.md#recover-and-abandon).

| Output | Meaning |
|---|---|
| `Nothing to recover.` | no interrupted batch (exit 0) |
| `Rolled back N interrupted batch(es).` | done (exit 0, or 1 if there were also errors) |
| `Nothing rolled back.` followed by `error:` lines | a batch could not be rolled back (exit 1) |

`--abandon <batch>` gives up on a batch recovery cannot handle: one that is interrupted, or committed
with damaged journal records. It records the batch as abandoned and changes nothing on disk. On a
terminal it asks `Abandon batch …? Its changes stay on disk as they are. [y/N]`. An unknown batch,
or one that needs no abandoning, is a usage error (exit 2).

A walk-through is in [Recover after a crash](use-cases/recover-after-a-crash.md).

## tpfs mcp

```text
tpfs mcp --root <dir>... [--approval terminal|client]
tpfs guard
tpfs doctor [<project>]
```

Runs the MCP server on stdin and stdout for an AI agent. Details are in [MCP server](mcp.md).

| Flag | Effect |
|---|---|
| `--root <dir>` | required, repeatable: the folders the agent may see and change |
| `--approval terminal` | default: a person approves each plan with `tpfs approve` |
| `--approval client` | the MCP client's own confirmation of the tool call is the only approval |

## tpfs pending

Lists the plans agents proposed that are waiting for a decision, oldest first:

```console
$ tpfs pending
18e97dcdaead317b5f2  waiting  3 op(s)  Put invoices in one folder
```

The state is `waiting` or `approved`. A stored plan that cannot be read shows as `DAMAGED`; remove it
with `tpfs reject`. Without plans it prints `No pending plans.`

## tpfs approve

```text
tpfs approve <planId>
```

Shows an agent's plan with its allowed folders, risk and digest, and asks
`Type 'yes' to let the agent apply exactly this plan: `. Only `yes` approves. It works only
interactively: there is no flag an agent could pass to approve its own plan.

| Result | Exit |
|---|---|
| `Approved. The agent can now call apply_plan for <id>.` | 0 |
| `approval must be given interactively in a terminal` | 4 |
| `not approved` (any answer but `yes`) | 4 |
| the plan is no longer valid (it lists `Problems:`) | 3 |
| unknown or invalid id | 2 |

## tpfs reject

```text
tpfs reject <planId>
```

Removes a pending plan, approved or not, so the agent can no longer apply it:
`Rejected 18e97dd0cbba59e268b.` A damaged pending file can be rejected too. For a day afterwards
the agent's `apply_plan` answers that the user rejected the plan (a `<planId>.rejected` marker in
the pending folder, removed after a day). Answering `no` in `tpfs watch` does the same.

## Environment variables

| Variable | Effect |
|---|---|
| `TPFS_HOME` | the state folder, unless `--home` is given |
| `XDG_STATE_HOME` | on Unix, the state folder is `$XDG_STATE_HOME/tpfs` if `TPFS_HOME` is not set |
| `LOCALAPPDATA` | on Windows, the state folder is `%LOCALAPPDATA%\tpfs` if `TPFS_HOME` is not set |
| `TPFS_ALLOW_UNCONFINED=1` | run where symlink-safe confinement is not available; see [Roots and confinement](concepts.md#roots-and-confinement) |
| `TPFS_ICONS` | any value but `0` turns on icons in the commander |
| `NO_COLOR` | no colour in previews |
| `VISUAL`, `EDITOR` | the editor the commander starts with F4; without them the first of `nano`, `vim`, `vi` on `PATH` (Notepad on Windows) |
| `SHELL` | the shell the commander's command line uses (default `sh`; on Windows `%COMSPEC%`, usually `cmd.exe`) |

## Windows notes

- Plans use Windows paths, for example `"C:\\Users\\you\\Downloads\\a.pdf"` (a backslash is doubled
  in JSON).
- The `tpfs approve` command an agent shows you is quoted for PowerShell.

## tpfs guard

```text
tpfs guard
```

A PreToolUse hook for AI coding agents: reads the proposed tool call as JSON on stdin and, for a
shell command that moves, renames or deletes files, prints a refusal that points the agent to tpfs.
Prints nothing for other commands. Always exits 0. See [Agent setup](agent-setup.md).

## tpfs doctor

```text
tpfs doctor [<project>]
```

Checks that an agent's setup for `<project>` (default: the current folder) routes file changes
through tpfs: the MCP server with terminal approval, the `tpfs guard` hook, no blanket shell allow
rule, a private state folder. Exit 0 when nothing failed, 1 otherwise. See
[Agent setup](agent-setup.md#check-the-setup).

## tpfs watch

```text
tpfs watch
```

Waits for plans proposed by agents and shows each one as it arrives, exactly as `tpfs approve`
does: the steps, the allowed folders, the risk and the digest. Answer `yes` to approve exactly that
plan, `no` to reject it, or press Enter to leave it pending. Plans that cannot be approved (they
have problems) are shown with a note. Runs until Ctrl+C. Needs a terminal (exit 4 without one).

```console
$ tpfs watch
Watching for plans proposed by agents (state: /home/you/.local/state/tpfs). Ctrl+C to stop.

Plan 18e98f3dd61777bec4a proposed by an agent:
  "Trash a"
  TRASH   /home/you/Downloads/a.txt
Total: 1 operation(s): 1 trash (2 B)
1 item(s) go to the trash (nothing is deleted; `tpfs undo` puts them back).
Allowed folders: /home/you/Downloads
Risk: HIGH — proposed by an agent
Digest: …
Type 'yes' to approve exactly this plan, 'no' to reject it, or Enter to decide later: yes
Approved. The agent can now call apply_plan for 18e98f3dd61777bec4a.
```

Recipes that agents propose are shown in the same loop, in the same way as
`tpfs recipe install` shows them. Answer `yes` to install the recipe, `no` to reject it, or press
Enter to decide later.

## tpfs recipes

```text
tpfs recipes
```

Lists the [recipes](recipes.md) that agents proposed and that wait for you, and the ones you
installed:

```console
$ tpfs recipes
Proposed by agents (review with `tpfs recipe install <id>`, or `tpfs recipe reject <id>`):
  18e99b8dfcc271573f8  "Receipts to accounting" v1 (receipts-to-accounting) from claude-code — You sort receipts by hand.
```

An installed recipe whose file was changed outside tpfs is listed as `DISABLED`. It cannot run
until it is installed again from a new proposal.

## tpfs recipe

```text
tpfs recipe install <id> [--root <dir>...]
tpfs recipe reject <id>
tpfs recipe show <id>
tpfs recipe run <recipe> <entry>... [--set <field>=<value>]... [--approve <digest>]
tpfs recipe remove <recipe>
```

### install

`install` shows the whole proposal and then asks you to type `yes`. The proposal includes the
client the agent named, its note, the folders the recipe may touch, what it takes, every form
field, the action template and the hash.

`--root` narrows the folders the recipe may work in. Each `--root` must lie inside the recipe's
own folders, and every folder must exist. Installing is interactive only (exit 4 without a
terminal). A proposal for an id that is already installed replaces it only if its version is
newer.

```console
$ tpfs recipe install 18e99b8dfcc271573f8
Recipe proposal 18e99b8dfcc271573f8 from claude-code (as the agent's client calls itself):
  Note: "You sort receipts by hand."
  Title: "Receipts to accounting" — receipts-to-accounting version 1
  Description: "Moves the selected receipts into the accounting folder, by year and month of the file."
  Folders it may touch: ~/Downloads, ~/Accounting
  Takes: 1–200 files named *.pdf, *.jpg, *.png
  Field --set supplier=…: "Supplier", text, 1–40 characters
  Field --set target=…: "Accounting folder", a folder inside the scope
  Action: move each to {form.target}/{item.mtime:%Y}/{item.mtime:%m}/{form.supplier|ascii|slug}_{item.mtime:%Y-%m-%d}_{n:02}.{item.ext|lower} (a taken name gets _2, _3 …)
  Hash: bb4bd284…
  It will work in: /home/you/Downloads, /home/you/Accounting
Type 'yes' to install this recipe (anything else leaves it waiting): yes
Installed receipts-to-accounting. Run it with `tpfs recipe run receipts-to-accounting <entry>…`.
```

### run

`run` expands an installed recipe over the entries you name and shows the plan, its risk and its
digest, in the same way as `tpfs apply`.

- **Form values.** Pass them with `--set field=value`. In a terminal, tpfs asks for any required
  field you left out. Without a terminal, a missing field is a usage error (exit 2).
- **Dates.** Modification times are taken in your time zone.
- **Rejected selections.** A selection the recipe does not accept is refused as a whole, with
  every problem listed (exit 3). Examples: a file outside its folders, a name its patterns do not
  match, or a taken name with `on_conflict: stop`.
- **The first three runs.** For these you type `yes`. After that, the question is the usual
  `Apply? [y/N]`. With `--approve <digest>`, it runs exactly the plan shown in an earlier run
  without asking.

```console
$ tpfs recipe run receipts-to-accounting ~/Downloads/invoice.PDF ~/Downloads/receipt.jpg --set 'supplier=Café Müller'
target ("Accounting folder", a folder inside the scope): /home/you/Accounting
Plan: Recipe "Receipts to accounting" v1: move 2 files
  MKDIR   /home/you/Accounting/2026
  MKDIR   /home/you/Accounting/2026/10
  MOVE    /home/you/Downloads/invoice.PDF  ->  /home/you/Accounting/2026/10/Cafe_Muller_2026-10-03_01.pdf
  MOVE    /home/you/Downloads/receipt.jpg  ->  /home/you/Accounting/2026/10/Cafe_Muller_2026-10-03_02.jpg
Total: 4 operation(s): 2 move (4 B), 2 mkdir
Risk: low
Digest: aa87a9b3…
This recipe is new to you (run 1 of its first 3): check every step.
Type 'yes' to run it: yes
Done: 4 step(s), batch 18e99b8e0de606b0-402. Undo with `tpfs undo`.
```

A run is an ordinary batch: `tpfs undo` reverts it.

### reject, show, remove

- `reject` discards a proposal; the agent sees `rejected` in `recipe_status`.
- `show` prints a proposal, or an installed recipe with the folders you confirmed and its run
  count.
- `remove` uninstalls a recipe.
