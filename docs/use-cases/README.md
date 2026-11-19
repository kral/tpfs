# Use cases

Each page shows one task from start to end: the goal, the plan or commands, the preview, what
happens, and how to undo it. All output comes from tpfs 0.4.0; your batch ids and digests will
differ.

| Use case | You learn |
|---|---|
| [Organise a downloads folder](organise-downloads.md) | write a plan with `mkdir` and `move`, preview it, apply it, undo it |
| [Clean up safely](clean-up-safely.md) | `trash` and `rmdir`, where trashed files go, and how undo restores them |
| [Mirror a folder](mirror-a-folder.md) | `tpfs sync` with a dry run, `--mirror`, `--apply`, and the empty-source guard |
| [Let an AI agent tidy files](agent-tidies-files.md) | the MCP flow: the agent proposes, you approve or reject, the agent applies |
| [Case study: the best-known AI file disasters, replayed through tpfs](case-study-agent-cleanup-incidents.md) | `rm -rf ~/` during a repo cleanup, and moves into a missing folder that overwrote every file |
| [Case study: an AI agent cleans up a deploy server](devops-release-cleanup.md) | DevOps: old releases and logs, approval in a terminal, one-command undo after an incident |
| [Recover after a crash](recover-after-a-crash.md) | `tpfs recover`, `tpfs log` and `tpfs recover --abandon` |
| [Work in the commander](work-in-the-commander.md) | copy, move, rename, trash and undo in the two-panel TUI |
