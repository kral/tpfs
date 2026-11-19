# Let an AI agent tidy files

**Goal:** let an AI agent sort the invoices in `~/Downloads`, while you decide what actually happens.
The agent proposes a plan over MCP; you review and approve it in your own terminal; then the agent
applies it. You also see how to reject a plan.

Setup and the tool reference are in [MCP server](../mcp.md).

## Setup

Add tpfs to your MCP client, with only the folder the agent should work in as its root:

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

Keep a terminal open next to the agent. That is where you approve.

## 1. The agent looks around

The agent calls `list_directory` with `{"path": "/home/you/Downloads"}`:

```json
{
  "entries": [
    {
      "name": "old-installers",
      "type": "dir"
    },
    {
      "name": "IMG_0412.jpg",
      "size": 4,
      "type": "file"
    },
    {
      "name": "IMG_0413.jpg",
      "size": 5,
      "type": "file"
    },
    {
      "name": "invoice-2026-03.pdf",
      "size": 8,
      "type": "file"
    },
    {
      "name": "invoice-2026-04.pdf",
      "size": 4,
      "type": "file"
    },
    {
      "name": "setup.zip",
      "size": 4,
      "type": "file"
    }
  ],
  "path": "/home/you/Downloads",
  "total": 6,
  "truncated": false
}
```

## 2. The agent proposes a plan

The agent calls `propose_plan`:

```json
{
  "plan": {
    "reason": "Put invoices in one folder",
    "ops": [
      { "op": "mkdir", "path": "/home/you/Downloads/Invoices" },
      { "op": "move", "from": "/home/you/Downloads/invoice-2026-03.pdf", "to": "/home/you/Downloads/Invoices/invoice-2026-03.pdf" },
      { "op": "move", "from": "/home/you/Downloads/invoice-2026-04.pdf", "to": "/home/you/Downloads/Invoices/invoice-2026-04.pdf" }
    ]
  }
}
```

tpfs validates it, stores it as pending, and answers with the preview, the risk and the command you
should run:

```json
{
  "digest": "4452696c13e56aed242c4b3321a035cfe2035d8472a8de00cc844b4a77e731a9",
  "next": "Show the user the preview and ask them to run `tpfs approve 18e97e4e998f5b2f2e6 --home /home/you/.local/state/tpfs` in a terminal (or to answer it in `tpfs watch`, if it is running). Then call apply_plan.",
  "planId": "18e97e4e998f5b2f2e6",
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

A plan from an agent is always HIGH risk. Nothing has changed on disk.

If the agent tries `apply_plan` now, it is told to wait:

```json
{
  "error": "not approved yet. Ask the user to run `tpfs approve 18e97e4e998f5b2f2e6 --home /home/you/.local/state/tpfs` in a terminal, then call apply_plan again."
}
```

## 3. You review and approve

In your terminal:

```console
$ tpfs pending
18e97e4e998f5b2f2e6  waiting  3 op(s)  Put invoices in one folder
$ tpfs approve 18e97e4e998f5b2f2e6 --home ~/.local/state/tpfs
Plan 18e97e4e998f5b2f2e6 proposed by an agent:
  "Put invoices in one folder"
  MKDIR   /home/you/Downloads/Invoices
  MOVE    /home/you/Downloads/invoice-2026-03.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-03.pdf
  MOVE    /home/you/Downloads/invoice-2026-04.pdf  ->  /home/you/Downloads/Invoices/invoice-2026-04.pdf
Total: 3 operation(s): 2 move (12 B), 1 mkdir
Allowed folders: /home/you/Downloads
Risk: HIGH — proposed by an agent
Digest: 4452696c13e56aed242c4b3321a035cfe2035d8472a8de00cc844b4a77e731a9
Type 'yes' to let the agent apply exactly this plan: yes
Approved. The agent can now call apply_plan for 18e97e4e998f5b2f2e6.
$ tpfs pending
18e97e4e998f5b2f2e6  approved 3 op(s)  Put invoices in one folder
```

Check that the digest matches the one the agent showed you. The approval is for exactly this plan.
`--home` is only needed when your terminal uses another state folder than the server; copying the
command the agent gives you is always safe.

`tpfs approve` works only in a terminal. Run from a script, or by the agent itself, it refuses:
`approval must be given interactively in a terminal` (exit 4).

## 4. The agent applies

The agent calls `apply_plan` with `{"planId": "18e97e4e998f5b2f2e6"}`:

```json
{
  "applied": 3,
  "batch": "18e97e4ebe557299-6301",
  "undo": "undo_last reverts it; the user can also run `tpfs undo`."
}
```

The approval is used up: the plan leaves `tpfs pending`, and the batch shows in your log with the
source `agent`:

```console
$ tpfs pending
No pending plans.
$ tpfs log
State: /home/you/.local/state/tpfs
18e97e4ebe557299-6301  agent  committed  3 step(s)  Put invoices in one folder
```

## 5. Undo

Either side can revert the batch. The agent calls `undo_last`:

```json
{
  "batch": "18e97e4ebe557299-6301",
  "steps": 3,
  "undone": true
}
```

You can run `tpfs undo` instead, or press Ctrl+Z in the [commander](../tui.md). An agent can undo
only a batch an agent made; if your own batch is newer, it gets an error and must ask you.

## Rejecting a plan

Say the agent proposes to trash `setup.zip` and you do not want that. Either answer anything but
`yes` when you approve:

```console
$ tpfs approve 18e97e4ec04e62e6309
…
Type 'yes' to let the agent apply exactly this plan: no
tpfs approve: not approved
```

The plan stays pending (exit code 4), and the agent keeps getting `not approved yet`. Or remove the
plan for good:

```console
$ tpfs reject 18e97e4ec04e62e6309
Rejected 18e97e4ec04e62e6309.
$ tpfs pending
No pending plans.
```

The agent's next `apply_plan` then fails:

```json
{
  "error": "plan '18e97e4ec04e62e6309' was rejected by the user"
}
```

Tell the agent why you rejected it, so it can propose something better.

## When the agent gets it wrong

A plan that breaks a rule never reaches you. tpfs returns it to the agent as an error, with the
problems:

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
