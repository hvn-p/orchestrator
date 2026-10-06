---
name: check-claude-code
description: Checks that orchestrator still works with the installed Claude Code. Reads Claude Code's changelog since the last verified version, runs the manual integration test, then records the new version or drafts an issue naming the broken contract. Use when a new Claude Code version is out or installed, or when asked to "check Claude Code compatibility", "/check-claude-code", "does orchestrator still work with the new Claude Code", "vérifie la compatibilité avec Claude Code", "nouvelle version de Claude Code", "Claude Code a été mis à jour". Never in CI.
---

# Check Claude Code compatibility

orchestrator relies on Claude Code only through the contracts listed in
`docs/claude-code-dependency.md`, each with an id. This skill checks them
against the installed Claude Code. It never changes the code: a broken
contract becomes an issue, and fixing it is a separate decision.

## Steps

1. **Versions.** Read the version on the `Last verified: Claude Code <version>,
   <date>.` line of `docs/claude-code-dependency.md`, then run
   `claude --version`. Same version: say so, and stop unless the user wants
   the test run anyway.

2. **Changelog.** The authoritative changelog is `CHANGELOG.md` at the root of
   <https://github.com/anthropics/claude-code>: one `## <version>` section per
   release, newest first. Print the sections newer than the verified version:

   ```sh
   gh api repos/anthropics/claude-code/contents/CHANGELOG.md \
     -H 'Accept: application/vnd.github.raw' |
     awk -v old=<verified version> '/^## / { if ($2 == old) exit; p = 1 } p'
   ```

   Sections above the installed version are releases not installed yet: skip
   them. Read each remaining entry against each contract, by meaning rather
   than keyword, and list those that may touch one, with its id.

3. **Integration test.** Run it with its report:

   ```sh
   cargo test --test claude_code -- --ignored --nocapture
   ```

   It starts a real headless session (Haiku, about a cent) and prints each
   contract as `ok` or `BROKEN`. When an orchestrated session may be running
   binaries from this repository's `target/`, set `CARGO_TARGET_DIR` to a
   scratch directory first. "Haiku did not run the probe" is a setup failure,
   not a broken contract: run it again.

4. **Every contract holds**, and no changelog entry casts doubt on one the
   test cannot see (each contract's Verified line says how it is verified):
   on a new branch from the default branch, set the `Last verified` line to
   the installed version and today's date, commit, and offer to push and open
   a pull request.

5. **Something broke.** Find which contract and why: the report's evidence,
   the changelog entries listed in step 2, Claude Code's documentation, and
   the code relying on the contract (`grep -rn 'claude-code: <id>'`). Draft a
   GitHub issue:
   - title: `Claude Code <version> breaks <contract id>`;
   - the installed and the last verified versions;
   - each failing contract with its report line;
   - the evidence, and the relevant changelog entries;
   - the suspected cause and fix, naming the marked code to change.

   Show the draft. Create it with `gh issue create` only once the user
   agrees, then stop.

## Rules

- Never fix the code or edit a contract from this skill.
- A changelog entry that changes a contract the test cannot see counts as a
  broken contract until verified otherwise.
- Never run in CI: the test needs a signed-in Claude Code and spends tokens.
