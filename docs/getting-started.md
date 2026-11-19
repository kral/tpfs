# Getting started

This page takes you from source code to your first undone change in a few minutes. For the ideas
behind each step, see [Concepts](concepts.md).

## Build and install

You need Rust 1.89 or newer (`rustc --version`).

```sh
git clone https://github.com/kral/tpfs
cd tpfs
cargo install --locked --path crates/tpfs
```

`cargo install` builds the release binary and copies it to `~/.cargo/bin/tpfs`. If you prefer to
keep it in the source tree, run `cargo build --release` and use `target/release/tpfs`.

Check it runs:

```console
$ tpfs --version
tpfs 0.4.0
```

`tpfs --help` lists every command. Each command also has its own short help, for example
`tpfs apply --help`.

## Write your first plan

Say your downloads folder looks like this:

```console
$ ls ~/Downloads
IMG_0412.jpg
IMG_0413.jpg
invoice-2026-03.pdf
invoice-2026-04.pdf
setup.zip
```

Save this as `organise.json`, with your own home folder in the paths:

```json
{
  "reason": "Sort downloads into folders",
  "ops": [
    { "op": "mkdir", "path": "/home/you/Downloads/Invoices" },
    { "op": "mkdir", "path": "/home/you/Downloads/Photos" },
    { "op": "move", "from": "/home/you/Downloads/invoice-2026-03.pdf", "to": "/home/you/Downloads/Invoices/invoice-2026-03.pdf" },
    { "op": "move", "from": "/home/you/Downloads/invoice-2026-04.pdf", "to": "/home/you/Downloads/Invoices/invoice-2026-04.pdf" },
    { "op": "move", "from": "/home/you/Downloads/IMG_0412.jpg", "to": "/home/you/Downloads/Photos/IMG_0412.jpg" },
    { "op": "move", "from": "/home/you/Downloads/IMG_0413.jpg", "to": "/home/you/Downloads/Photos/IMG_0413.jpg" }
  ]
}
```

Paths are absolute, and `to` is the full new path, not the folder to drop the file into. The steps
run in order, so a step may use a folder that an earlier step creates. The full format is in
[Plan format](plan-format.md).

## Preview it

`tpfs check` validates the plan and shows what it would do. It never changes anything.

```console
$ tpfs check organise.json --root ~/Downloads
Plan: Sort downloads into folders
  MKDIR   /home/you/Downloads/Invoices
  MKDIR   /home/you/Downloads/Photos
  MOVE    /home/you/Downloads/invoice-2026-03.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-03.pdf
  MOVE    /home/you/Downloads/invoice-2026-04.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-04.pdf
  MOVE    /home/you/Downloads/IMG_0412.jpg  ->  /home/you/Downloads/Photos/IMG_0412.jpg
  MOVE    /home/you/Downloads/IMG_0413.jpg  ->  /home/you/Downloads/Photos/IMG_0413.jpg
Total: 6 operation(s): 4 move (21 B), 2 mkdir
Risk: low
Digest: af624d8ac50c3d4153f2e93a5df7c11206cb5d04458e296f5e450f228bd0263d
OK — the plan is valid.
```

`--root` names the folder the plan may touch. Every path outside it is refused. You can give
`--root` more than once.

The digest identifies this exact plan. Your digest will differ, because it covers your paths.

## Approve and execute it

`tpfs apply` shows the same preview and asks before it changes anything:

```console
$ tpfs apply organise.json --root ~/Downloads
Plan: Sort downloads into folders
  …
Risk: low
Digest: af624d8ac50c3d4153f2e93a5df7c11206cb5d04458e296f5e450f228bd0263d
Apply? [y/N] y
Done: 6 step(s), batch 18e97ddd3e09330c-6c01. Undo with `tpfs undo`.
```

A high-risk plan asks you to type the full word `yes` instead.

In a script there is nobody to answer. Pass the digest you reviewed instead, and tpfs applies the
plan only if it still has exactly that digest:

```sh
tpfs apply organise.json --root ~/Downloads --approve af624d8ac50c3d4153f2e93a5df7c11206cb5d04458e296f5e450f228bd0263d
```

## See what happened

```console
$ tpfs log
State: /home/you/.local/state/tpfs
18e97ddd3e09330c-6c01  human  committed  6 step(s)  Sort downloads into folders
```

`tpfs log <batch>` lists every step of one batch and how each is undone.

## Undo it

```console
$ tpfs undo
Undo batch 18e97ddd3e09330c-6c01 by human — 6 step(s): Sort downloads into folders
These changes will be reversed:
  MKDIR   /home/you/Downloads/Invoices
  …
Undo it? [y/N] y
Undone batch 18e97ddd3e09330c-6c01 (6 step(s)):
  reverted: MKDIR   /home/you/Downloads/Invoices
  …
$ ls ~/Downloads
IMG_0412.jpg
IMG_0413.jpg
invoice-2026-03.pdf
invoice-2026-04.pdf
setup.zip
```

Run `tpfs undo` again to revert the batch before that. When nothing is left, it says
`Nothing to undo.`

## Where to go next

- Learn the model behind this: [Concepts](concepts.md).
- Work with the keyboard instead of JSON: [Commander (TUI)](tui.md).
- Keep a backup folder in step: [Mirror a folder](use-cases/mirror-a-folder.md).
- Let an AI agent tidy files for you: [MCP server](mcp.md).
