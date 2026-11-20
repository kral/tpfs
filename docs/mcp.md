# MCP server

`tpfs mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server that gives an AI
agent safe hands for files. The agent can look around inside the folders you allow and **propose** a
plan. The plan runs only after **you approve** it, by default with `tpfs approve` in your own
terminal, a channel the agent does not control. The approval is bound to the plan's
[digest](concepts.md#digest), and everything the agent does can be undone.

## Run the server

```sh
tpfs mcp --root ~/Downloads
```

The server speaks newline-delimited JSON-RPC on stdin and stdout and prints
`tpfs mcp: serving on stdio` on stderr. Your MCP client starts it; you do not run it by hand.

| Flag | Effect |
|---|---|
| `--root <dir>` | required, repeatable: the folders the agent may list and change |
| `--approval terminal` | default: you approve each plan with `tpfs approve <planId>` |
| `--approval client` | the client's confirmation of the `apply_plan` call is the only approval |
| `--home <dir>` | the [state folder](concepts.md#the-state-folder) |

`--approval client` is weaker: if you set your client to "always allow" the `apply_plan` tool, there
is no human approval at all. Use it only if your client always asks you.

The server supports MCP protocol versions `2025-06-18`, `2025-03-26` and `2024-11-05`.

## Configure your client

Most MCP clients take a server entry like this. Use absolute paths; the client may start the server
from any folder.

```json
{
  "mcpServers": {
    "tpfs": {
      "command": "/home/you/.cargo/bin/tpfs",
      "args": ["mcp", "--root", "/home/you/Downloads"]
    }
  }
}
```

Add more `"--root", "<dir>"` pairs to allow more folders. Keep the roots narrow: the agent can see
and propose changes to everything inside them.

The server stores pending plans in its state folder, and `tpfs approve` must use the same one. The
server tells the agent the exact command to give you, including `--home`, so this works even if the
client starts the server with another `TPFS_HOME`.

## The flow

1. The agent calls `list_directory` to find exact paths.
2. The agent calls `propose_plan`. tpfs validates it, rates the risk (always HIGH for an agent) and
   stores it as pending. Nothing changes yet.
3. The agent shows you the preview and asks you to run `tpfs approve <planId> --home …`.
4. You review the plan in your terminal and type `yes`, or reject it with `tpfs reject <planId>`.
   With `tpfs watch` running in a terminal, each plan appears there as it is proposed and you
   answer `yes`, `no` or Enter (later) without copying its id.
5. The agent calls `apply_plan`. The plan runs as one batch, journaled like any other.
6. If needed, the agent calls `undo_last`, or you run `tpfs undo`.

### As the agent sees it

On `initialize`, the server tells the agent the rules and the allowed folders:

```text
Safe, undoable file operations. You cannot change files directly: describe the change as a plan with propose_plan, show the user the returned preview and risk, and wait for their approval. After proposing, ask the user to review and approve it by running `tpfs approve <planId> --home /home/you/.local/state/tpfs` in their terminal, then call apply_plan. Nothing is ever overwritten or deleted permanently ('trash' is undoable). Allowed folders: /home/you/Downloads.
```

A valid proposal returns:

```json
{
  "digest": "4452696c13e56aed242c4b3321a035cfe2035d8472a8de00cc844b4a77e731a9",
  "next": "Show the user the preview and ask them to run `tpfs approve 18e97dcdaead317b5f2 --home /home/you/.local/state/tpfs` in a terminal (or to answer it in `tpfs watch`, if it is running). Then call apply_plan.",
  "planId": "18e97dcdaead317b5f2",
  "preview": [
    "MKDIR   /home/you/Downloads/Invoices",
    "MOVE    /home/you/Downloads/invoice-2026-03.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-03.pdf",
    "MOVE    /home/you/Downloads/invoice-2026-04.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-04.pdf"
  ],
  "previewTruncated": false,
  "risk": "High",
  "riskReasons": [
    "proposed by an agent"
  ],
  "stepCount": 3,
  "valid": true
}
```

When `apply_plan` cannot run a plan, its error carries a `status` and a `next` step, so an agent
does not have to read the sentence: `not_approved` (wait, then call again), `rejected` (ask the
user what to change; do not propose the same plan again), `unknown` (applied already or never
proposed), `modified` (the stored plan was changed; propose it again) or `used` (it was started
once already, maybe interrupted by a crash; an approval is for one run only). Calling `apply_plan`
before you approved returns:

```json
{
  "error": "not approved yet. Ask the user to run `tpfs approve 18e97dcdaead317b5f2 --home /home/you/.local/state/tpfs` in a terminal, then call apply_plan again.",
  "next": "Wait for the user's decision, then call apply_plan again. Do not propose the plan again meanwhile.",
  "planId": "18e97dcdaead317b5f2",
  "status": "not_approved"
}
```

After approval, `apply_plan` returns:

```json
{
  "applied": 3,
  "batch": "18e97dcee5687f76-6612",
  "undo": "undo_last reverts it; the user can also run `tpfs undo`."
}
```

### As you see it

```console
$ tpfs pending
18e97dcdaead317b5f2  waiting  3 op(s)  Put invoices in one folder
$ tpfs approve 18e97dcdaead317b5f2
Plan 18e97dcdaead317b5f2 proposed by an agent:
  "Put invoices in one folder"
  MKDIR   /home/you/Downloads/Invoices
  MOVE    /home/you/Downloads/invoice-2026-03.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-03.pdf
  MOVE    /home/you/Downloads/invoice-2026-04.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-04.pdf
Total: 3 operation(s): 2 move (12 B), 1 mkdir
Allowed folders: /home/you/Downloads
Risk: HIGH — proposed by an agent
Digest: 4452696c13e56aed242c4b3321a035cfe2035d8472a8de00cc844b4a77e731a9
Type 'yes' to let the agent apply exactly this plan: yes
Approved. The agent can now call apply_plan for 18e97dcdaead317b5f2.
```

`tpfs approve` validates the plan again before it asks. It only works in a terminal; there is no flag
to approve non-interactively, so an agent that can run commands still cannot approve its own plan.

To refuse a plan, answer anything but `yes` (the plan stays pending), or remove it:

```console
$ tpfs reject 18e97dd0cbba59e268b
Rejected 18e97dd0cbba59e268b.
```

The agent then gets this from `apply_plan`:

```json
{
  "error": "plan '18e97dd0cbba59e268b' was rejected by the user",
  "next": "Do not propose the same plan again. Ask the user what to change, then propose a new plan.",
  "planId": "18e97dd0cbba59e268b",
  "status": "rejected"
}
```

## Tools

| Tool | Arguments | What it does |
|---|---|---|
| `list_directory` | `path`, optional `limit` (1–5000, default 500) | lists a folder inside the roots: name, type (`dir`, `file`, `symlink`) and size of files; folders first |
| `propose_plan` | `plan` | validates a [plan](plan-format.md) without running it; stores it and returns `planId`, `digest`, `risk`, `riskReasons`, `preview`, `stepCount` and `next` |
| `apply_plan` | `planId` | runs an approved plan; returns `applied` (steps) and `batch` |
| `undo_last` | none | reverts the newest batch, only if an agent made it |
| `history` | none | the agent's 20 most recent batches: `batch`, `at` (Unix seconds), `source`, `status`, `steps`, `reason` |
| `recipe_schema`, `recipe_validate`, `recipe_propose`, `recipe_status` | see [Recipes](recipes.md) | offer a reusable command that you install and run yourself; the agent never runs it |

Rules the agent meets:

- **Unknown arguments are errors**, both for tools and inside a plan. A made-up option such as
  `force` is refused, so the agent learns that it does not exist:

  ```json
  {
    "error": "not a valid plan: unknown field `force`, expected `ops` or `reason`"
  }
  ```

- **An invalid plan** is returned as a tool error with `valid: false`, up to 50 `issues`, and the
  first 20 preview lines:

  ```json
  {
    "issueCount": 1,
    "issues": [
      "#2: Outside the allowed folders: /home/you/photos/IMG_0412.jpg"
    ],
    "issuesTruncated": false,
    "preview": [
      "TRASH   /home/you/Downloads/setup.zip",
      "MOVE    /home/you/Downloads/IMG_0412.jpg  ->  /home/you/photos/IMG_0412.jpg"
    ],
    "previewTruncated": false,
    "stepCount": 2,
    "valid": false
  }
  ```

- **A plan is applied once.** If execution fails, the plan is used up and must be proposed again.
  If the stored plan was changed on disk after it was proposed, `apply_plan` refuses it.
- **Limits:** a reason of at most 2000 characters, at most 10 000 operations, at most 100 plans
  waiting at once, a preview of at most 200 lines, and JSON-RPC messages of at most 4 MiB.
- **An agent can only undo an agent's batch.** For a newer batch of yours it gets
  ``the most recent batch was made by a human; only they can undo it (`tpfs undo`)``.
- **`history` shows only agent batches**, because your own batches may name folders outside the
  agent's roots.
- `list_directory` hides tpfs's own work files and refuses to list through a symlink.

Statuses in `history`: `committed`, `rolled back`, `undone`, `interrupted` (ask the user to run
`tpfs recover`), `damaged` (ask the user to check `tpfs log`), `abandoned`.

## Undo and recovery

Batches made by the agent are in the same journal as yours. You see them in `tpfs log` with the
source `agent`, and `tpfs undo` reverts them like any other batch:

```console
$ tpfs log
State: /home/you/.local/state/tpfs
18e97dcee5687f76-6612  agent  committed  3 step(s)  Put invoices in one folder
```

The server opens the journal for each call, so `tpfs undo`, `tpfs recover` and the commander keep
working while it runs.

A full walk-through is in [Let an AI agent tidy files](use-cases/agent-tidies-files.md).
