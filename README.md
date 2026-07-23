# Forge Composer

A first-party, FOSS, Cursor-Composer-class agent surface for VSCodium, built on
[forgeloop](https://github.com/NOVATechnocrat) physics.

Two components:

- **`daemon/`** — `composerd` (Rust): the brain. Event-sourced multi-agent sessions
  (append-only JSONL ledgers), an orchestrator agent with full agency over dispatched
  subagents, a multi-provider gateway (Fireworks / Ollama / Anthropic / OpenAI, explicit
  model pins, BYOK), git-worktree-per-subagent execution, a deny-before-allow command
  policy engine, and a read-only bridge to the forgeloop Judge. Sessions survive editor
  restarts; any client can attach.
- **`extension/`** — the VSCodium extension (TypeScript): a thin client over localhost
  HTTP+SSE. Composer chat with clickable file:line links, diff review, a live orchestration
  board, session takeover, pause/steer/inject controls. Holds no keys, no model logic, no
  session state.

## Principles

- **Single chain of command, no invisible interventions.** Human → orchestrator →
  subagents. Every actor's action — the human's included — is an event on the session
  ledger; the orchestrator sees human interdictions in-stream.
- **Verdicts are journals, not narration.** "Done" means the forgeloop Judge's sealed
  oracle passed, and the verdict event cites the journal path.
- **Explicit model pins, never auto-pools.** Sovereign routing is the point.
- **Security is structural.** Provenance-tagged prompt framing against injection, argv-level
  command deny-lists, worktree jails, secret redaction, bearer-token localhost API, and no
  code path to the oracle-minting machinery.

## Design of record

- Spec: [`docs/superpowers/specs/2026-07-22-forge-composer-design.md`](docs/superpowers/specs/2026-07-22-forge-composer-design.md)
- Roadmap: M0 Spine → M1 Hands → M2 Orchestra → M3 Physics → M4 Ship (spec §12)

## Status

M0 (Spine) + M1 (Hands) + M2 (Orchestra) + M3 (Physics) loop-gated. Dogfood
via `bash scripts/dogfood-codium.sh` (isolated VSCodium profile — does not
touch your Cursor session).

M4 (Ship — packaging, docs, first-run experience) is next.

## License

Apache-2.0. Patterns distilled from Cline (Apache-2.0) and Kilo/OpenCode (Apache-2.0/MIT)
are credited where vendored; see file headers.
