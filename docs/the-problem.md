# The problem tpfs solves: AI agents that destroy files

This page sums up the most common complaint about AI coding and file agents on Reddit
(r/ClaudeAI, r/cursor), the Cursor forum, Hacker News and the incident trackers, as of November
2026: **an agent asked to tidy, move or clean up files deletes or overwrites far more than it was
asked to, and there is no way back.** Stack Overflow has little on it: people do not ask how to do
it, they report it after it happened, in vendor forums and on Reddit.

## What keeps happening

### 1. A cleanup becomes a recursive delete of the wrong tree

The best-known case (December 2025, r/ClaudeAI, "Claude CLI deleted my entire home directory"): a
user asked Claude Code to clean up an old repository. The agent ran

```sh
rm -rf tests/ patches/ plan/ ~/
```

and the trailing `~/` expanded to the home directory. The post reached 240 points on Hacker News.
Variants of the same failure are reported again and again:

- Cursor's agent asked to delete the files of one session ran `rmdir /s /q` and wiped a whole
  `D:` drive, about 80 GB, which did not go to the Recycle Bin.
- On Windows, broken nested quoting in `cmd /c "rmdir /s /q …"` passed `\` (the drive root) as a
  second target.
- An agent asked to delete one project hit an error, opened another terminal and deleted the user's
  work folder instead.
- The repository and everything else in its parent folder disappeared during normal work, with no
  delete instruction at all.

### 2. A move into a folder that does not exist overwrites everything

Gemini CLI, July 2025 (AI Incident Database #1178): asked to reorganise a folder, the agent's
`mkdir` failed silently, the agent treated it as a success, and moved every file to the
non-existent path. On Windows that renames each file to the same name, so each one overwrote the
previous one; one file survived. The agent never ran a check after its commands.

### 3. Files vanish quietly

Threads such as "AI agent secretly deleting my files" (r/cursor) and "Quiet file deletion" (Cursor
forum) describe whole folders or important docs disappearing during unrelated work, noticed only
later through broken imports or a staged-changes check.

### 4. The safety nets have holes

- **Approval per shell command is weak.** In one Cursor report the user was approving each delete
  one by one, and one of the approved commands was `rm -rf ~/`. Auto-run ("YOLO") modes skip the
  step entirely.
- **Agent checkpoints only cover their own editing tools.** Claude's Agent SDK documents that
  changes made through Bash are not captured; Cursor's restore checkpoint does not reverse terminal
  commands either.
- **Git only knows what was committed.** Uncommitted work, untracked files and anything outside the
  repository are gone.
- **`rm -rf` and `rmdir /s /q` bypass the trash.** What is left is file-recovery software
  (PhotoRec, disk scanners) with partial results.
- Even "undo" buttons in agent tools have deleted whole repositories (a known Cursor issue).

## The root cause

The agent gets a **shell with irreversible primitives** (`rm -rf`, `del /s`, `mv` over existing
names) and acts **step by step without a reviewable whole**. Nothing checks the outcome of one step
before the next, nothing stops a path from expanding outside the project, and nothing records how to
put things back. The human sees, at best, one command at a time.

The usual advice (commit often, deny-list `rm -rf`, sandbox the agent, keep backups) treats the
symptoms: deny-lists are bypassed by another spelling (`find -delete`, PowerShell
`Remove-Item -Recurse`), sandboxes and containers do not help when the files to tidy are your real
files, and backups are only as fresh as the last run.

## How tpfs answers each failure

| Failure seen in the wild | What happens with tpfs |
|---|---|
| `rm -rf … ~/` expands to the home directory | There is no delete and no shell. A plan names exact paths; anything outside the `--root` folders, or through a symlink, is refused before anything runs. |
| `rmdir /s /q` hits the drive root, nothing in the Recycle Bin | `trash` is the only way to take something away: the item moves to tpfs's trash and `tpfs undo` puts it back. A root folder cannot be trashed at all. |
| Move into a folder that does not exist overwrites every file | Validation simulates the whole plan first: a move whose target folder does not exist is refused, and no operation ever replaces an existing entry (no-replace renames). |
| A failed step is treated as a success | Each step is checked again right before it runs; a failed step stops the batch and the steps before it are rolled back. |
| Quiet deletions noticed days later | Every batch is in `tpfs log`: who proposed it (person or agent), why, each step and its undo. |
| One-command-at-a-time approval misses the dangerous one | The agent proposes the **whole plan**; a person approves exactly that plan, by its digest, in their own terminal. Agent plans are always HIGH risk. |
| Checkpoints miss shell changes | When tpfs is the agent's only file tool (its shell file commands refused by `tpfs guard`, see [Agent setup](agent-setup.md)), every file change goes through the journal and is undoable. |
| A crash or reboot mid-operation | The write-ahead journal records each step's inverse before the step; `tpfs recover` rolls an interrupted batch back. |

What tpfs does not cover: changes an agent makes through other tools it is given (a shell, an
editor tool), file contents edited in place, and anything outside the file system (databases,
services). It is the safe path for moving, copying, renaming and removing files; give the agent
that path instead of a shell for those jobs.

See [the incidents replayed through tpfs](use-cases/case-study-agent-cleanup-incidents.md) and the
[DevOps case study](use-cases/devops-release-cleanup.md) for the full flow on real tasks.

## Sources

- Reddit r/ClaudeAI: "Claude CLI deleted my entire home directory! Wiped my whole mac."
  <https://www.reddit.com/r/ClaudeAI/comments/1pgxckk/claude_cli_deleted_my_entire_home_directory_wiped/>
- Docker blog, "Coding agent horror stories: the rm -rf incident"
  <https://www.docker.com/blog/coding-agent-horror-stories-the-rm-rf-incident/>
- Hacker News discussion of the same incident <https://hn.nuxt.dev/item/46268222>
- Cursor forum: "Agent wiped D: drive instead of deleting one folder"
  <https://forum.cursor.com/t/agent-wiped-d-drive-instead-of-deleting-one-folder/169919>
- Cursor forum: "Windows Agent rmdir broken quoting wiped D:"
  <https://forum.cursor.com/t/windows-agent-rmdir-broken-quoting-wiped-d-ticket-t-f90960-grok-4-6-extra-high/172345>
- Cursor forum: "Cursor AI agent deleted all my work files"
  <https://forum.cursor.com/t/cursor-ai-agent-deleted-all-my-work-files/164251>
- Cursor forum: "Cursor/Claude Code deleted my entire Documents folder on macOS"
  <https://forum.cursor.com/t/cursor-claude-code-deleted-my-entire-documents-folder-on-macos/167086>
- Cursor forum: "Critical safety incident" (approved deletes, then `rm -rf ~/`)
  <https://forum.cursor.com/t/critical-safety-incident-seeking-fair-resolution-improved-safeguards/99351>
- Cursor forum: "Quiet file deletion" <https://forum.cursor.com/t/quiet-file-deletion/110838>
- Cursor forum: "Undo changes deleted entire repo" <https://forum.cursor.com/t/undo-changes-deleted-entire-repo/128551>
- Reddit r/cursor: "Ai agent secretly deleting my files"
  <https://www.reddit.com/r/cursor/comments/1k1h24a/ai_agent_secretly_deleting_my_files/>
- AI Incident Database #1178, Gemini CLI deletes user files <https://incidentdatabase.ai/cite/1178>
- Adversa AI, "Nine AI coding agent incidents that ended with deleted data"
  <https://adversa.ai/blog/ai-coding-agent-incidents/>
- File checkpointing in the Claude Agent SDK (Bash changes not tracked)
  <https://hexdocs.pm/claude_code/0.19.0/file-checkpointing.html>
