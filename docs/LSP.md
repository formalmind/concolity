# Findings in the editor: concolity × solidity-language-server

The language server (github.com/asyncswap/solidity-language-server) already has what this needs: diagnostics, code
lenses, inlay hints, hover, code actions and `workspace/executeCommand`, on solc's AST. The Formal Mind desktop app
runs it too (the Code tab's Neovim, and `lsp.rs` for definition and hover). So what concolity and the swarm's agents
learn can sit on the lines it's about, in any editor, without a new protocol.

```mermaid
flowchart LR
  subgraph Producers
    C[concolity coverage / explore]
    A[swarm agents: report_bug, record_finding, tag]
  end
  C --> F[.formal/findings.sarif<br/>.formal/coverage.json]
  A --> F
  F -->|didChangeWatchedFiles| L[solidity-language-server]
  L --> E1[diagnostics]
  L --> E2[code lens per function]
  L --> E3[inlay hints on branches]
  L --> E4[hover]
  L --> E5[code actions]
  E5 -->|executeCommand| C
  F --> B[desktop Bugs page]
```

## One file, three readers

Producers write **SARIF 2.1.0** to `.formal/findings.sarif`. It's the standard for analysis results: GitHub code
scanning shows it on PRs, the VS Code SARIF viewer reads it, and the desktop Bugs page can read it as well.
Coverage goes next to it as concolity's JSON (`coverage --json`), which per file has line hits and the hit counts of
both sides of every JUMPI.

A result carries:

| SARIF field | What goes in it |
|---|---|
| `ruleId` | `concolity/panic-0x11`, `concolity/dead-guard`, `concolity/missing-bytecode`, `swarm/finding`, `swarm/tag` |
| `level` | `error` (a finding with a failing test), `warning`, `note` |
| `locations` | `src/AsyncSwap.sol:446`, the narrowest statement |
| `message` | one sentence, as a report states it |
| `properties.inputs` | the solved arguments (`["1592598101", "0", "…"]`) and the harness (`MagicExploreTest::testFuzz_poke`) |
| `properties.evidence` | the revert reason, the named writes, the PoC path |
| `relatedLocations` | the PoC test, the Lean theorem that proves a guard dead |
| `fingerprints` | file + function + rule, so a result survives edits above it |

## Built (2026-10-25)

- concolity writes `--sarif .formal/findings.sarif` from coverage and explore (one run per command, merged).
- solidity-language-server, branch `claude/sarif-findings`: results at error / warning / note become diagnostics
  (`source` = the tool, `code` = the rule; note shows as information), and every result on a line, the `none` ones
  too (solved and untaken sides), shows under the symbol's hover. A watcher on `**/.formal/findings.sarif`
  republishes the open files when it changes (in editors that support registered file watchers).

```sh
concolity coverage . --sarif .formal/findings.sarif && concolity explore . --only <harness> --sarif .formal/findings.sarif
```

## Built (2026-10-31): coverage in the editor, bundled

solidity-language-server (main, from v0.1.38) depends on concolity as a crate and runs coverage itself, behind the
`coverage` setting (off by default; `coverage.enabled`, `dimUncovered`, `functionSummary`, `runOnSave`). With it on:

- the first Solidity file opened in a project loads the suite's last run from concolity's cache
  (`~/.cache/formalmind/coverage`, keyed by the git tree; `coverage::latest_map`), or runs the suite in the
  background (`run_map`), then republishes every open file. Once the files change, the last run is shown and every
  note says it's from an earlier run;
- a line no test runs is a hint diagnostic tagged `unnecessary`, so the editor dims it (Neovim's
  `DiagnosticUnnecessary`, VS Code's faded text); a covered line with a side no test takes is a hint saying which
  side and how often the other ran. Point 1 below stands: a side is never "unnecessary", a line nobody runs is;
- hover on a line: how often it ran and each branch side's counts;
- an inlay hint after each function's header: `12/14 lines · 20/26 sides` (function spans from tree-sitter, so it
  needs no solc build);
- `solidity.coverage.refresh` runs the suite again; `coverage.runOnSave` runs it after each save.

The cache gained line counts for this (`<k>.lines.json` next to the branch map), and sol-debug's terminal UI became a
feature so the server builds the runner without ratatui.

## What the language server shows

1. **Diagnostics.** Each result becomes a `Diagnostic` with `code` = `ruleId` and `source` = `concolity` or `swarm`, and
   its related locations as `relatedInformation` (built, without related locations yet). An unreachable side is a
   note (information), not `DiagnosticTag.Unnecessary`: the line still runs, only one of its sides doesn't.
2. **Code lens over each function** (the server knows function ranges from the AST): `lines 12/14 · sides 5/8 · 2
   solved · 1 dead · 1 finding`. Clicking it runs `concolity.explore` scoped to that function.
3. **Inlay hints on branch lines**: `⟂ false side never taken`, `solved: x = 0x5eed1e55`, `dead (unsat, 3 harnesses)`.
4. **Hover** on a line: how often it ran, both sides' counts, the explore outcome with its inputs and harness, and the
   finding's text with its evidence.
5. **Code actions**:
   - *Debug with these inputs*: runs `concolity debug … --arg …`, or opens the desktop debugger on that run.
   - *Write a PoC test*: a Foundry test with the inputs as constants, under `test/`.
   - *Explore this branch*: runs explore with this side as the only target.
   - *Acknowledge*: writes the result's suppression into the SARIF file, with a reason.
6. **`workspace/executeCommand`**: `concolity.coverage`, `concolity.explore { function }`, `concolity.debug { test, args }`.
   The server shells out to the `concolity` binary and republishes when the SARIF file changes.

## Agents tag code

The swarm's agents already file findings (`report_bug`, `record_finding`). Add a `tag` tool: an agent marks a line or a
function with a short note and a kind (`suspicion`, `assumption`, `invariant`, `todo`), for example "rounding here favours the
taker", "assumes the PoolManager is unlocked", "orderCount must equal the nodes in the level". Tags go to the SARIF file
as `swarm/tag` notes. The person sees them inline in the editor, can promote one to a finding or dismiss it, and the
next agent reads the open tags on the function before working on it.

## Work, by repository

| Repository | Work |
|---|---|
| concolity | `--sarif <path>` on coverage and explore (findings, dead guards, missing bytecode); per-function rollups in the JSON |
| formalmind desktop | the swarm's `report_bug` / `record_finding` / a new `tag` write SARIF too; the Bugs page reads it |
| solidity-language-server | watch `.formal/*.sarif` and `.formal/coverage.json`; diagnostics, lenses, inlay hints, hover, code actions and the three commands above |

The order that pays first: concolity's `--sarif`, then diagnostics plus hover in the language server. Those two put every
finding and dead guard on its line. Lenses, hints and actions build on the same data.
