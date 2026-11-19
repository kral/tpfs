# Recover after a crash

**Goal:** a plan that files 150 scans into year folders is cut short by a crash or a power loss. Put
the disk back into a known state, and learn what to do when the journal itself was damaged.

Background: [The journal](../concepts.md#the-journal) and
[Recover and abandon](../concepts.md#recover-and-abandon).

## What a crash leaves behind

The plan `scans.json` creates `2019` and `2020` in `~/archive` and moves 150 scans into them. The
process was killed after a few steps:

```console
$ ls ~/archive | head -4
2019
2020
scan-005.pdf
scan-006.pdf
$ tpfs log
State: /home/you/.local/state/tpfs
18e97de3176d02ec-6d3b  human  INCOMPLETE (run `tpfs recover`) 7 step(s)  File scans by year
```

Half the work is done. The journal knows exactly which steps finished, because every step was
recorded, with its inverse, before it touched the disk:

```console
$ tpfs log 18e97de3176d02ec-6d3b | tail -4
  #6    done     MOVE    /home/you/archive/scan-003.pdf  ->  /home/you/archive/2020/scan-003.pdf
         undo:    MOVE    /home/you/archive/2020/scan-003.pdf  ->  /home/you/archive/scan-003.pdf
  #7    done     MOVE    /home/you/archive/scan-004.pdf  ->  /home/you/archive/2019/scan-004.pdf
         undo:    MOVE    /home/you/archive/2019/scan-004.pdf  ->  /home/you/archive/scan-004.pdf
```

A step that was cut off in the middle shows as `started`.

## tpfs waits for you

Until you deal with the interrupted batch, tpfs refuses to undo or to apply new plans, because
their changes would mix with the half-done batch:

```console
$ tpfs undo
tpfs undo: batch 18e97de3176d02ec-6d3b was interrupted; run `tpfs recover` first (it rolls it back), then undo
```

## Recover

```console
$ tpfs recover
Rolled back 1 interrupted batch(es).
$ ls ~/archive | head -3
scan-000.pdf
scan-001.pdf
scan-002.pdf
$ tpfs log
State: /home/you/.local/state/tpfs
18e97de3176d02ec-6d3b  human  rolled back 7 step(s)  File scans by year
```

The scans are back where they were and the two new folders are gone. Now apply the plan again from
the start.

If recovery itself is interrupted or stops on an error, fix the cause and run `tpfs recover` again.
It records each step it rolls back, so it continues where it stopped. When there is nothing left:

```console
$ tpfs recover
Nothing to recover.
```

## When the journal is damaged

Sometimes the crash also damages the journal: a power loss can leave a broken line. tpfs detects
that with a checksum on every line. A batch with unreadable records is marked `DAMAGED`:

```console
$ tpfs log
State: /home/you/.local/state/tpfs
18e97de4eaba61d8-6e58  human  DAMAGED (see `tpfs log <batch>`) 4 step(s)  File scans by year
warning: 1 unreadable journal line(s) skipped
```

tpfs no longer knows for certain which steps ran, so it does not roll the batch back by itself:

```console
$ tpfs undo
tpfs undo: batch 18e97de4eaba61d8-6e58 is interrupted and its journal records are damaged; see `tpfs log`, then `tpfs recover --abandon 18e97de4eaba61d8-6e58` if you accept the disk as it is
$ tpfs recover
Nothing rolled back.
  error: batch 18e97de4eaba61d8-6e58: journal records are damaged; not rolled back automatically: see `tpfs log 18e97de4eaba61d8-6e58`, then `tpfs recover --abandon 18e97de4eaba61d8-6e58` to accept the disk as it is
```

Running `tpfs recover` again will not help here. Instead:

1. See what the batch did, as far as the journal can tell:

   ```console
   $ tpfs log 18e97de4eaba61d8-6e58 | head -8
   Batch 18e97de4eaba61d8-6e58 by human (journal records damaged: the list may be incomplete): File scans by year
     #1    done     MKDIR   /home/you/archive/2019
            undo:    RMDIR   /home/you/archive/2019
     #2    done     MKDIR   /home/you/archive/2020
            undo:    RMDIR   /home/you/archive/2020
     #3    done     MOVE    /home/you/archive/scan-000.pdf  ->  /home/you/archive/2019/scan-000.pdf
            undo:    MOVE    /home/you/archive/2019/scan-000.pdf  ->  /home/you/archive/scan-000.pdf
     #5    started  MOVE    /home/you/archive/scan-002.pdf  ->  /home/you/archive/2019/scan-002.pdf
   ```

   Step #4 is missing: its record was the damaged line.

2. Look at the disk and fix by hand what you need, for example with a new plan later.

3. Accept the disk as it is, so tpfs can move on:

   ```console
   $ tpfs recover --abandon 18e97de4eaba61d8-6e58
   Abandon batch 18e97de4eaba61d8-6e58? Its changes stay on disk as they are. [y/N] y
   Abandoned batch 18e97de4eaba61d8-6e58; its changes were left as they are.
   ```

`--abandon` changes nothing on disk. It only records that you gave up on the batch:

```console
$ tpfs log
State: /home/you/.local/state/tpfs
18e97de4eaba61d8-6e58  human  abandoned (changes left on disk) 4 step(s)  File scans by year
warning: 1 unreadable journal line(s) skipped
$ tpfs recover
Nothing to recover.
```

New plans work again. Undo skips the abandoned batch.

## Other cases for --abandon

- **Newer batches on top.** If other batches were committed after the interrupted one, recover does
  not roll it back, because its inverses would run over those later changes. It says so and names
  `tpfs log <batch>` and `tpfs recover --abandon <batch>`.
- **A committed batch with damaged records.** Undo refuses it and tells you to look at
  `tpfs log <batch>`, undo by hand what you need, and then abandon it.

Abandoning a batch that is fine is refused:
`batch … is neither interrupted nor damaged; nothing to abandon` (exit 2). A batch abandoned already
gives `batch … is already abandoned; nothing to do` (exit 2).
