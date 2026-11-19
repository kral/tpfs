# Mirror a folder

**Goal:** keep `~/backup` an exact copy of `~/photos`. Preview what a sync would do, apply it, and
see how tpfs protects you when the source is missing or empty.

`tpfs sync` only builds a plan of `copy` and `trash` operations. It goes through the same
validation, preview, approval, journal and undo as any other plan. All flags are in
[`tpfs sync`](../cli.md#tpfs-sync).

## Starting point

```text
~/photos                     ~/backup
  2026/beach.jpg               2026/forest.jpg   (same as in photos)
  2026/forest.jpg              2026/removed.jpg  (only in backup)
  list.txt                     list.txt          (same time, other size)
  notes.txt   (new)            notes.txt         (old)
```

## Dry run

Without `--apply`, `tpfs sync` only shows the plan:

```console
$ tpfs sync ~/photos ~/backup
4 difference(s), 1 identical file(s).
  skip  2026/removed.jpg  (only on the right; mirror is off)
  skip  list.txt  (same time, different size; decide by hand)
Plan: Sync /home/you/photos → /home/you/backup: 1 to copy, 1 to replace (old versions to trash)
  COPY    /home/you/photos/2026/beach.jpg  ->  /home/you/backup/2026/beach.jpg
  TRASH   /home/you/backup/notes.txt
  COPY    /home/you/photos/notes.txt  ->  /home/you/backup/notes.txt
Total: 3 operation(s): 2 copy (6 B), 1 trash (4 B)
1 item(s) go to the trash (nothing is deleted; `tpfs undo` puts them back).
Risk: MEDIUM — replaces 1 existing file(s); the current versions go to the trash
Digest: fc26cb1024c3b7d2b9b27899a9bf48d1ff421c66bba3980bfe4d4f0a3cbf8362
Dry run. Apply with --apply, or non-interactively with --approve fc26cb1024c3b7d2b9b27899a9bf48d1ff421c66bba3980bfe4d4f0a3cbf8362
```

What it means:

- `beach.jpg` is missing in the backup: it is copied.
- `notes.txt` is newer in `photos`: the old copy goes to the trash, then the new one is copied. This
  is why the risk is MEDIUM: an existing file is replaced, and you can get the old version back.
- `removed.jpg` exists only in the backup. A plain sync leaves it alone.
- `list.txt` has the same time and a different size. tpfs cannot tell which one is right, so it
  never touches it. Decide by hand.

## Mirror

`--mirror` makes the target an exact copy: what exists only in the target goes to the trash.

```console
$ tpfs sync ~/photos ~/backup --mirror
4 difference(s), 1 identical file(s).
  skip  list.txt  (same time, different size; decide by hand)
Plan: Sync /home/you/photos → /home/you/backup: 1 to copy, 1 to replace (old versions to trash), 1 to trash (mirror)
  COPY    /home/you/photos/2026/beach.jpg  ->  /home/you/backup/2026/beach.jpg
  TRASH   /home/you/backup/2026/removed.jpg
  TRASH   /home/you/backup/notes.txt
  COPY    /home/you/photos/notes.txt  ->  /home/you/backup/notes.txt
Total: 4 operation(s): 2 copy (6 B), 2 trash (6 B)
2 item(s) go to the trash (nothing is deleted; `tpfs undo` puts them back).
Risk: MEDIUM — replaces 1 existing file(s); the current versions go to the trash
Digest: 887a79949cea02038b324e05290938686b66f08c99583f0c052d73c7cee49e14
Dry run. Apply with --apply, or non-interactively with --approve 887a79949cea02038b324e05290938686b66f08c99583f0c052d73c7cee49e14
```

`--mirror` works one way only; with `--both` it is refused.

## Apply

`--apply` shows the same preview and asks:

```console
$ tpfs sync ~/photos ~/backup --mirror --apply
…
Digest: 887a79949cea02038b324e05290938686b66f08c99583f0c052d73c7cee49e14
Apply? [y/N] y
Done: 4 step(s), batch 18e97de14dfe52fd-6d02. Undo with `tpfs undo`.
```

In a script, pass the digest from the dry run instead:
`tpfs sync ~/photos ~/backup --mirror --approve <digest>`. If anything changed in either folder
since the dry run, the plan is different, its digest no longer matches, and nothing happens.

Running the sync again shows that only the conflict is left:

```console
$ tpfs sync ~/photos ~/backup --mirror
1 difference(s), 3 identical file(s).
  skip  list.txt  (same time, different size; decide by hand)
Nothing to sync.
```

## Undo

The whole sync is one batch. `tpfs undo` removes the copies (to the trash) and puts the replaced
and mirrored files back:

```sh
tpfs undo
```

## The empty-source guard

A disk that is not mounted often leaves an empty folder behind. A mirror from that empty folder
would plan to trash everything in the backup. tpfs refuses to plan that:

```console
$ tpfs sync ~/usb ~/backup --mirror
3 difference(s), 0 identical file(s).
  skip  2026  (its source folder is empty (is a disk not mounted?); a mirror would trash it — not planned)
  skip  list.txt  (its source folder is empty (is a disk not mounted?); a mirror would trash it — not planned)
  skip  notes.txt  (its source folder is empty (is a disk not mounted?); a mirror would trash it — not planned)
Nothing to sync.
```

How the guard works:

- It applies only to the mirror's trash operations. Copies are planned as usual.
- It looks at the folder in the source that corresponds to each item's parent. If that folder is
  empty, the target-only items in it are not trashed. This works at every level, not only at the top.
- OS metadata files such as `.DS_Store`, `Thumbs.db`, `desktop.ini` or `._*` do not count: a
  folder with only those is empty.
- A source folder that does not exist at all is an error (exit 2), never treated as empty:

  ```console
  $ tpfs sync ~/nothere ~/backup --mirror
  tpfs sync: /home/you/nothere: No such file or directory (os error 2)
  ```


If the source really is empty and you want the backup emptied, trash the items with an ordinary plan.

## Review the plan as JSON

`--json` prints the plan instead of the preview, with a summary on stderr. You can keep it, review
it, and apply it later:

```console
$ tpfs sync ~/photos ~/backup --json > sync.json
1 operation(s), 1 skipped, 3 identical
$ cat sync.json
{
  "ops": [
    {
      "op": "copy",
      "from": "/home/you/photos/2026/lake.jpg",
      "to": "/home/you/backup/2026/lake.jpg"
    }
  ],
  "reason": "Sync /home/you/photos → /home/you/backup: 1 to copy"
}
$ tpfs apply sync.json --root ~/photos --root ~/backup --approve 464a23d3b0888c8776993113cee73cb35ad5566f3a5e593b89a22cfe98a0c0dd
Plan: Sync /home/you/photos → /home/you/backup: 1 to copy
  COPY    /home/you/photos/2026/lake.jpg  ->  /home/you/backup/2026/lake.jpg
Total: 1 operation(s): 1 copy (5 B)
Risk: low
Digest: 464a23d3b0888c8776993113cee73cb35ad5566f3a5e593b89a22cfe98a0c0dd
Done: 1 step(s), batch 18e97de15f42ce02-6d15. Undo with `tpfs undo`.
```

This is also the way to sync more than 10 000 operations: `tpfs apply` accepts `--max-ops`,
`tpfs sync` does not.
