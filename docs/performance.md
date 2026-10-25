# Performance and resource use

Budgets come from `plan.md` §5.12. Measurements: macOS 26 on Apple silicon, release build,
2026-10-25.

| Item | Budget | Measured |
| --- | --- | --- |
| Desktop installer (`.dmg`) | ≤ 40 MB | 14.1 MB |
| Desktop app bundle (`.app`, CLI included) | — | 30.4 MB |
| CLI binary | ≤ 30 MB | 13.5 MB |
| Desktop app idle memory (app process + WebKit content, GPU and network processes, physical footprint) | ≤ 150 MB | ≈ 109 MB (26 + 54 + 22 + 7) |
| Idle mock server (PostgreSQL / MySQL / Redis), physical footprint | ≤ 15 MB | 2.7 / 3.4 / 2.8 MB |

How the app stays light:

- One Rust process hosts the core, all mock servers and the MCP endpoint; mocks are tasks, not
  processes.
- The data grid virtualizes rows and columns, so only visible cells exist in the DOM.
- Tables load in pages with server-side sorting and filtering; query results are capped by the row
  limit setting.
- The ER diagram code loads on first use.
- Mock storage is SQLite with WAL; preview twins are temporary engines that copy only affected rows.

To measure on macOS: `footprint -p <pid>` for each process; sizes from the bundle output of
`npm run package` (in `apps/desktop`).
