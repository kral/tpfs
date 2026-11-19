# Plan format

A plan is a JSON object with an ordered list of operations and an optional reason. `tpfs schema`
prints its JSON Schema (draft 2020-12). What validation checks on top of the format is in
[Validation](concepts.md#validation).

```json
{
  "reason": "Sort downloads into folders",
  "ops": [
    { "op": "mkdir", "path": "/home/you/Downloads/Invoices" },
    { "op": "move", "from": "/home/you/Downloads/invoice-2026-03.pdf", "to": "/home/you/Downloads/Invoices/invoice-2026-03.pdf" }
  ]
}
```

## The plan object

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ops` | array of operations | yes | the steps, run in this order |
| `reason` | string | no (default `""`) | a short description, shown in previews and stored in the journal |

Any other field is an error. An agent that invents an option such as `"force": true` must learn
that it does not exist, instead of believing it took effect:

```console
$ echo '{"ops":[], "force": true}' | tpfs check - --root ~/Downloads
tpfs check: not a valid plan: unknown field `force`, expected `ops` or `reason` at line 1 column 18
```

## Operations

Each operation is an object with an `op` field and the fields of that operation, no more. All paths
are absolute.

| `op` | Fields | Effect |
|---|---|---|
| `move` | `from`, `to` | move a file or folder to the full path `to` |
| `copy` | `from`, `to` | copy a file or folder (recursively) to the full path `to`; never overwrites |
| `rename` | `path`, `newName` | rename in place; `newName` is a bare name, not a path |
| `trash` | `path` | move a file or folder to the journaled [trash](concepts.md#the-trash) |
| `mkdir` | `path` | create a folder; its parent must exist (or be created earlier in the plan) |
| `rmdir` | `path` | remove an empty folder |

`to` is the **full destination path**, not the folder to drop the item into. To move `a.pdf` into
`Invoices`, write `"to": "/home/you/Downloads/Invoices/a.pdf"`. If you name an existing folder by
mistake, validation tells you:

```text
#1: Target already exists: /home/you/Downloads/Photos ('to' is the full new path: to put it inside that folder, use /home/you/Downloads/Photos/IMG_0412.jpg)
```

`rename` keeps the item in its folder. To change the letter case of a name only (`photo.JPG` to
`photo.jpg`), use `rename`; it works on file systems that ignore case too.

### Examples

```json
{ "op": "move",   "from": "/home/you/Downloads/IMG_0412.jpg", "to": "/home/you/Pictures/IMG_0412.jpg" }
{ "op": "copy",   "from": "/home/you/photos", "to": "/home/you/backup/photos" }
{ "op": "rename", "path": "/home/you/Downloads/IMG_0412.jpg", "newName": "beach.jpg" }
{ "op": "trash",  "path": "/home/you/Downloads/setup.zip" }
{ "op": "mkdir",  "path": "/home/you/Downloads/Invoices" }
{ "op": "rmdir",  "path": "/home/you/Downloads/old-installers" }
```

## Order matters

The operations run one after another, and validation simulates them in the same order:

- `mkdir X` followed by `move a → X/a` is valid; the other way round is not.
- Moving the same file twice is not valid: after the first move, the source no longer exists.
- To replace a file, trash it first, then copy or move the new one to its path. tpfs rates this as
  a replacement (MEDIUM risk) and the old version stays in the trash.

## Paths

- On Unix: `"/home/you/Downloads/a.pdf"`. On Windows: `"C:\\Users\\you\\Downloads\\a.pdf"`
  (a backslash is doubled in JSON).
- `..` and `.` are resolved before anything else; the preview shows the resolved path.
- A `~` is not expanded. Write the full path.
- Names must be valid Unicode.

## Limits

| Limit | Value |
|---|---|
| operations per plan | 10 000 (raise with `--max-ops` for your own plans) |
| reason | 10 000 characters (2000 for plans from an agent) |
| path | 4096 bytes, at most 256 folders deep |
| plan file | 64 MiB |

## Generating plans

A plan is plain JSON, so any script can write one. `tpfs sync --json` writes the plan of a folder
comparison, which you can review, edit and apply:

```sh
tpfs sync ~/photos ~/backup --json > sync.json
tpfs check sync.json --root ~/photos --root ~/backup
tpfs apply sync.json --root ~/photos --root ~/backup
```

See [Mirror a folder](use-cases/mirror-a-folder.md).
