# Set up an AI agent so its file changes go through tpfs

tpfs only protects the file operations that go through it. An agent that also has a shell can
still run `rm -rf` or `mv` past it, which is how most of the [reported incidents](the-problem.md)
happened. A safe setup has three parts:

1. **The tpfs MCP server**, so the agent can look around and propose plans.
2. **The `tpfs guard` hook**, which refuses shell commands that move, rename or delete files and
   tells the agent to use tpfs instead.
3. **No blanket "allow every shell command"** rule, which would let commands run unchecked.

Then run `tpfs doctor` to check it, and keep `tpfs watch` open in a terminal while the agent works:
each plan it proposes appears there for your `yes` or `no`.

## Claude Code

### 1. The MCP server

In the project, `.mcp.json`:

```json
{
  "mcpServers": {
    "tpfs": {
      "command": "/home/you/.cargo/bin/tpfs",
      "args": ["mcp", "--root", "/home/you/projects/shop"]
    }
  }
}
```

Keep the roots to the folders the task needs. Leave out `--approval client`: with the default, each
plan is approved by you in a terminal (`tpfs approve <planId>`), a channel the agent does not
control.

### 2. The guard hook

In the project, `.claude/settings.json` (or `~/.claude/settings.json` for every project):

```json
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "tpfs guard" } ] }
    ]
  }
}
```

Use the full path to `tpfs` if it is not on the `PATH` Claude Code sees. Claude Code runs the hook
before every shell command and passes the call as JSON on stdin. `tpfs guard` answers with a
refusal for commands that change files, and nothing for anything else:

```console
$ echo '{"tool_name":"Bash","tool_input":{"command":"rm -rf tests/ patches/ plan/ ~/"}}' | tpfs guard
{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"`rm` changes files outside tpfs. Move, rename or remove files with the tpfs MCP tools instead (propose_plan): the user reviews the plan, and it can be undone."}}
```

The agent reads the reason and switches to `propose_plan`.

What it refuses: `rm`, `rmdir`, `mv`, `unlink`, `shred`, `truncate`, and on Windows `del`, `erase`,
`rd`, `move`, `ren`, `Remove-Item`, `Move-Item`, `Rename-Item`, `Clear-Content`; also behind
`sudo`, `env`, `xargs`, inside `sh -c`/`bash -c`/`cmd /c`/`powershell -Command`, in `$(…)` and
backticks, `find -delete` and `find -exec rm`, `git clean`, `rsync --delete`, and one-line
`python -c`/`node -e`/`perl -e`/`ruby -e` scripts that remove or rename files. Everything else
(builds, tests, `git status`, reading files) passes.

It is a guard rail, not a sandbox: a program the agent writes and then runs can still change files
in ways no command line shows. For that, run the agent in a sandbox where only tpfs can write to
the folders that matter.

### 3. No blanket shell allow

Check that no settings file has `"Bash"` (or `"Bash(*)"`) in `permissions.allow`. With it every
shell command runs without asking, and Claude Code has been reported to ignore hook decisions in
that case. Allow specific commands instead, for example `"Bash(cargo test:*)"`.

Deny rules are a useful second layer, for example:

```json
{ "permissions": { "deny": ["Bash(rm:*)", "Bash(mv:*)", "Bash(rmdir:*)"] } }
```

## Cursor

Cursor runs a project's Claude Code hooks, so the `.claude/settings.json` hook above applies to
Cursor's agent too. Configure the MCP server in `.cursor/mcp.json` (same shape as `.mcp.json`), and
keep `rm`, `mv`, `del` and similar commands out of the terminal allowlist in
`~/.cursor/permissions.json`, so they never run without your approval.

## Check the setup

```console
$ tpfs doctor ~/projects/shop
ok    /home/you/projects/shop/.mcp.json (tpfs): tpfs mcp, approval in a terminal
ok    /home/you/projects/shop/.claude/settings.json: `tpfs guard` hook covers shell commands
ok    state folder /home/you/.local/state/tpfs is private

The agent's file changes go through tpfs and need your approval.
```

`tpfs doctor [<project>]` (default: the current folder) looks at `.mcp.json`, `~/.claude.json`,
`.cursor/mcp.json`, the Claude Code settings files and `~/.cursor/permissions.json`. It reports:

| Finding | Level |
|---|---|
| no tpfs MCP server configured | FAIL |
| `--approval client` (the client's confirmation is the only approval) | warn |
| a very broad root (the home folder or a drive) | warn |
| no `tpfs guard` hook for the shell | FAIL |
| `"Bash"` allowed in `permissions.allow` | FAIL |
| `rm`, `mv`, `del`… in Cursor's terminal allowlist | FAIL |
| the state folder is not private | FAIL |

It exits 0 when nothing failed and 1 otherwise, so it can run in a script or CI.
