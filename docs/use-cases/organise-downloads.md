# Organise a downloads folder

**Goal:** put invoices and photos from `~/Downloads` into their own folders, check the result, and
take it back.

## Starting point

```console
$ ls ~/Downloads
IMG_0412.jpg
IMG_0413.jpg
invoice-2026-03.pdf
invoice-2026-04.pdf
setup.zip
```

## The plan

Save as `organise.json`. The folders are created first, so the moves after them can use them.
Each `to` is the full new path.

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

## Preview

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

Nothing has changed yet. The risk is low: six operations, nothing trashed.

## Execute

```console
$ tpfs apply organise.json --root ~/Downloads
Plan: Sort downloads into folders
  …
Risk: low
Digest: af624d8ac50c3d4153f2e93a5df7c11206cb5d04458e296f5e450f228bd0263d
Apply? [y/N] y
Done: 6 step(s), batch 18e97ddd3e09330c-6c01. Undo with `tpfs undo`.
$ ls -R ~/Downloads
/home/you/Downloads:
Invoices
Photos
setup.zip

/home/you/Downloads/Invoices:
invoice-2026-03.pdf
invoice-2026-04.pdf

/home/you/Downloads/Photos:
IMG_0412.jpg
IMG_0413.jpg
```

The six steps ran as one batch. `tpfs log 18e97ddd3e09330c-6c01` lists each step and its undo:

```console
$ tpfs log 18e97ddd3e09330c-6c01
State: /home/you/.local/state/tpfs
Batch 18e97ddd3e09330c-6c01 by human: Sort downloads into folders
  #1    done     MKDIR   /home/you/Downloads/Invoices
         undo:    RMDIR   /home/you/Downloads/Invoices
  #2    done     MKDIR   /home/you/Downloads/Photos
         undo:    RMDIR   /home/you/Downloads/Photos
  #3    done     MOVE    /home/you/Downloads/invoice-2026-03.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-03.pdf
         undo:    MOVE    /home/you/Downloads/Invoices/invoice-2026-03.pdf  ->  /home/you/Downloads/invoice-2026-03.pdf
  …
```

## Undo

```console
$ tpfs undo
Undo batch 18e97ddd3e09330c-6c01 by human — 6 step(s): Sort downloads into folders
These changes will be reversed:
  MKDIR   /home/you/Downloads/Invoices
  MKDIR   /home/you/Downloads/Photos
  MOVE    /home/you/Downloads/invoice-2026-03.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-03.pdf
  MOVE    /home/you/Downloads/invoice-2026-04.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-04.pdf
  MOVE    /home/you/Downloads/IMG_0412.jpg  ->  /home/you/Downloads/Photos/IMG_0412.jpg
  MOVE    /home/you/Downloads/IMG_0413.jpg  ->  /home/you/Downloads/Photos/IMG_0413.jpg
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

Undo moved the files back and removed the two folders, which were empty again.

## In a script

Without a terminal, approve by digest and undo by batch id:

```console
$ tpfs apply organise.json --root ~/Downloads --approve af624d8ac50c3d4153f2e93a5df7c11206cb5d04458e296f5e450f228bd0263d
…
Done: 6 step(s), batch 18e97ddd620dcbd3-6c1a. Undo with `tpfs undo`.
$ tpfs undo
tpfs undo: not interactive: pass --approve 18e97ddd620dcbd3-6c1a to undo exactly that batch (see `tpfs log`)
$ tpfs undo --approve 18e97ddd620dcbd3-6c1a
Undone batch 18e97ddd620dcbd3-6c1a (6 step(s)):
  …
```

If the plan file changed after you noted the digest, `apply` refuses with exit code 4.
