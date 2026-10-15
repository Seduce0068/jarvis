# JARVIS Architecture Documentation

This directory contains architecture decision records and resource calculations for the JARVIS Personal AI system.

## Structure

```
docs/architecture/
├── AGENTS.md           # Agent instructions for this directory
├── report.md           # Main architecture report (Markdown + LaTeX + MPE imports)
├── calc.rs             # All calculations in Rust (run with rust-script)
├── verify.rs           # Rust verification script (run before committing)
├── dist/               # Generated artifacts (gitignored)
│   ├── report.md       # Calculated Markdown for sharing
│   └── report.html     # HTML export for sharing
└── src/                # Additional Rust sources (if needed)
```

## Quick Start

### Prerequisites

1. **Rust toolchain** (for `rust-script`):
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source ~/.cargo/env
   cargo install rust-script
   ```

2. **VS Code / code-server** with **Markdown Preview Enhanced** extension

3. **Optional**: `markdownlint` for linting
   ```bash
   npm install -g markdownlint-cli
   ```

### Workflow

1. **Edit calculations**: Modify `calc.rs`
2. **Verify locally**: `rust-script calc.rs`
3. **Edit document**: Modify `report.md` (formulas in LaTeX, import via `@import`)
4. **Preview**: Open `report.md` in MPE, run Code Chunk (`Shift+Enter`)
5. **Verify**: `rust-script verify.rs .`
6. **Share**: MPE → Save as Markdown → `dist/report.md`, or Export HTML → `dist/report.html`

### MPE Settings (workspace-only)

This repo includes `.vscode/settings.json` enabling script execution **only for this workspace**:
```json
{
  "markdown-preview-enhanced.enableScriptExecution": true,
  "markdown-preview-enhanced.mathRenderingOption": "KaTeX"
}
```

**Never enable globally** — see `AGENTS.md` for security rules.

## Current Report

The `report.md` analyzes **OCI Always Free (A1 Flex: 2 OCPU, 12 GB)** resource utilization for all JARVIS services:

| Service | OCPUs | Memory (GB) | Disk (GB) |
|---------|-------|-------------|-----------|
| jarvis-policy | 0.25 | 1.0 | 0.5 |
| jarvis-mcp-gw | 0.25 | 1.0 | 0.5 |
| jarvis-event | 0.15 | 0.5 | 0.3 |
| jarvis-context | 0.25 | 1.5 | 2.0 |
| opencode | 0.50 | 2.0 | 1.0 |
| code-server | 0.30 | 1.5 | 1.0 |
| postgresql | 0.20 | 2.0 | 10.0 |
| qdrant | 0.15 | 1.0 | 5.0 |
| otelcol | 0.05 | 0.3 | 0.1 |
| system/os | 0.10 | 1.0 | 5.0 |
| **Total** | **2.20** | **11.8** | **25.4** |

**Result**: ⚠️ The conservative sum exceeds the OCPU limit (calculated:
OCPU 110.0%, Memory 98.3%, Disk 12.7%). See `report.md` §5 for the verdict,
interpretation, and mitigations.

## Security

- **MPE script execution is a separate boundary from OpenCode permissions**
- Default: disabled globally, enabled only via `.vscode/settings.json` for this workspace
- Never run Code Chunks from untrusted Markdown
- Never use `modify_source=true` or `run_on_save=true`

## References

- [Markdown + MPE + Rust Architecture Report](../../markdown_mpe_rust_latex_architecture_report_2026-10-09.md)
- [ADR-0001: Unified Architecture](../../adr/0001-unified-architecture.md)
- [MPE Code Chunk Docs](https://github.com/shd101wyy/markdown-preview-enhanced/blob/master/docs/code-chunk.md)
- [rust-script](https://github.com/fornwall/rust-script)
