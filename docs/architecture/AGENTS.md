# Architecture Documentation - Agent Instructions

## Project: JARVIS Architecture Calculations

This directory (`docs/architecture/`) follows the **Markdown + MPE + Rust +
LaTeX** workflow defined in
[mpe-rust-latex-architecture-report.md](mpe-rust-latex-architecture-report.md).

---

## Source Files

| File | Purpose |
|------|---------|
| `report.md` | Authoritative document source (Markdown + LaTeX formulas + MPE imports) |
| `calc.rs` | All calculations in Rust (runnable via `rust-script`) |
| `verify.rs` | Optional local checks (skips cleanly when tool is absent) |
| `dist/` | Generated artifacts ONLY (never edit directly) |

---

## Rules for Agents

### Document Authoring
- **Treat `report.md` as the single source of truth** for narrative, formulas, and structure.
- **Implement ALL calculations in `calc.rs`** — no magic numbers in Markdown.
- **Use LaTeX math syntax** (`$...$`, `$$...$$`) for formulas in `report.md`.
- **Reference `calc.rs` via MPE `@import`** — do not duplicate logic in Markdown.
- **Keep `dist/` separate** — generated files go to `dist/`, never commit to main branch.

### Calculation Integrity
- **Validate before committing**: Run `rust-script calc.rs` and verify output matches expected values.
- **Cross-check**: Compare Rust output with independent calculation (spreadsheet, bc, python).
- **No guessed values**: Never replace computed results with manually typed approximations.
- **Version lock**: Pin Rust toolchain via `rust-toolchain.toml` when Cargo project is introduced.

### MPE Security Boundaries
- **MPE script execution is a SEPARATE security boundary from OpenCode permissions**.
- **Default**: `markdown-preview-enhanced.enableScriptExecution: false` in User settings.
- **Exception**: Enable ONLY for this workspace via `.vscode/settings.json`.
- **Never** run Code Chunks from untrusted Markdown files.
- **Never** enable `modify_source=true` or `run_on_save=true` in Code Chunks.

### Verification Protocol
Before marking any task complete in this directory:

1. `rust-script calc.rs` → exits 0, output matches `report.md` tables
2. `git diff report.md calc.rs` → only intentional changes
3. `rust-script verify.rs` → optional local checks (skips cleanly when the tool is absent)
4. MPE preview renders formulas and tables correctly
5. `dist/report.md` (Save as Markdown) renders on GitHub without MPE
6. No secrets or API keys in any file

### Prohibited Actions
- ❌ Rewriting unrelated files in this directory
- ❌ Publishing `dist/` artifacts to main branch
- ❌ Running unverified Code Chunks
- ❌ Hardcoding calculated values in `report.md`
- ❌ Enabling automatic script execution globally

---

## Commands

```bash
# Verify calculations
cd /home/koppe/ドキュメント/作業用/jarvis/jarvis/docs/architecture
rust-script calc.rs

# Optional local verification (skips cleanly if the tool is absent)
rust-script verify.rs

# Check Markdown lint
markdownlint report.md

# View git diff
git diff report.md calc.rs
```

---

## Workflow Integration

This directory is part of the JARVIS monorepo. The root `AGENTS.md` governs overall repo workflows. This file only covers architecture documentation specifics.

Cross-references:
- Root: `../../AGENTS.md`
- ADR: `../../adr/0001-unified-architecture.md`
- Infra: `../../infra/`
