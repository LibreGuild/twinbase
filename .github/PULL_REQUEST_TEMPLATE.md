## What and why
<!-- What does this change, and why? Link the issue: "Fixes #123". -->

## How it was tested
<!-- Tests added or run, and manual checks (which OS, which databases). Screenshots for UI changes. -->

## Checklist
- [ ] `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] `npm run typecheck`, `npm test` and `npm run build` pass in `apps/desktop` (UI changes)
- [ ] Tests cover the change (wire-protocol changes: a test with a real driver)
- [ ] Docs updated where behaviour changed (`README.md`, `docs/`), and `CHANGELOG.md` under *Unreleased*
- [ ] No credentials, tokens or private hosts in code, tests or docs
