---
description: Run every tpfs review agent, fix what they find, and repeat until each agent scores at least 95 with no blocking findings.
argument-hint: "[agent names to limit the run, or a git range such as main..HEAD]"
---

Review **tpfs** with its review agents and drive the result to the quality bar.

## Agents

| Agent | Area |
|---|---|
| `fs-safety-reviewer` | no overwrite, no permanent delete, correct undo and rollback |
| `crash-consistency-reviewer` | write-ahead journal, fsync ordering, recover |
| `security-reviewer` | confinement to roots, approval by digest, agents cannot self-approve, sanitising |
| `cross-platform-reviewer` | Linux / macOS / Windows behaviour, CI matrix |
| `rust-quality-reviewer` | panics, errors, API, unsafe, dependencies, binary size |
| `test-reviewer` | tests prove the guarantees |
| `mcp-reviewer` | MCP protocol conformance and agent usability |
| `cli-tui-ux-reviewer` | CLI and two-panel commander UX |

Arguments: `$ARGUMENTS` — if it names agents, run only those; if it is a git range, tell every agent to
focus on that diff (but still check invariants the diff can affect); if empty, review the whole
workspace.

## Bar

Every agent must report **`SCORE ≥ 95`, an empty `BLOCKING` list and `VERDICT: PASS`**. No averaging:
one agent below the bar means the review fails.

## Procedure

1. Make sure the tree builds: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`,
   `cargo test --workspace --locked --no-fail-fast`. Check CI on all platforms:
   `gh run list -R kral/tpfs -L 3`.
2. Launch the agents **in parallel**, each with: the scope (whole workspace or the range), the latest CI
   result, and the instruction to answer in its required output shape only.
3. Collect the reports into one table: agent, score, number of blocking / non-blocking / missing tests,
   verdict.
4. **Verify before fixing.** For each blocking finding, check it against the code yourself (reproduce
   with a test where possible). Drop findings that do not hold and note why. Duplicates across agents
   count once.
5. Fix the confirmed blocking findings and the missing tests for them; add a regression test for every
   fixed defect. Keep fixes minimal and in the style of the surrounding code. Then fix non-blocking
   findings that are cheap and clearly right.
6. Re-run step 1, then re-run **only the agents that were below the bar or whose area the fixes
   touched**, giving them the previous report so they check the fixes.
7. Repeat 4–6 until every agent passes, or stop after **3 rounds** and report what is left and why.
8. Final report: the table with final scores, what was fixed (one line each, with commit), what was
   rejected as not a real issue, and anything left open. Commit fixes in focused commits; do not push
   unless asked.

Agents never change files — only you do, after verifying.
