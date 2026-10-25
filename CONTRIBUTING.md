# Contributing to TwinBase

Thanks for helping! Bug reports, ideas, documentation and code are all welcome. Please follow the
[Code of Conduct](CODE_OF_CONDUCT.md) in every interaction.

## Ways to help
- **Report a bug:** [open an issue](https://github.com/LibreGuild/twinbase/issues/new/choose) with the
  steps to reproduce it, what you expected, the database or driver involved, and your system.
- **Suggest a feature:** start with a [discussion](https://github.com/LibreGuild/twinbase/discussions/categories/ideas)
  or a feature request, so the idea can be shaped before anyone writes code.
- **Ask a question:** use [Discussions → Q&A](https://github.com/LibreGuild/twinbase/discussions/categories/q-a).
- **Report a security problem:** never in a public issue. See [SECURITY.md](SECURITY.md).
- **Send a pull request:** fixes and small improvements can go straight to a PR. For anything big (a
  new database, a new mock protocol, a change to the workspace or bundle format), open an issue
  first so we agree on the approach.

## Set up
You need **Rust** stable 1.90+ ([rustup](https://rustup.rs)) and **Node.js** 22+, plus:
- **macOS:** Xcode command line tools (`xcode-select --install`)
- **Windows:** Visual Studio Build Tools with "Desktop development with C++", and WebView2 (already
  on Windows 10 and 11)
- **Linux (Debian/Ubuntu):** `sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev libssl-dev patchelf file`

Then:
```bash
git clone https://github.com/LibreGuild/twinbase.git
cd twinbase/apps/desktop
npm ci
npx tauri dev        # the desktop app, with hot reload of the UI
```

[docs/development.md](docs/development.md) explains running the UI in a normal browser, the test
suites and the tests that run against real databases in Docker.

## Before you open a pull request
Run the same checks as CI:
```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cd apps/desktop
npm run typecheck
npm test
npm run build
```

## Guidelines
- **Read [docs/architecture.md](docs/architecture.md) first.** It explains where things belong. Every
  feature goes through the one core API (`twinbase_core::api::call`), so the UI, MCP and CLI share it.
- **Match the code around you** and keep changes focused; unrelated refactors belong in their own PR.
- **Add tests** for behaviour you add or fix. Wire-protocol changes need a test with a real driver.
- **Keep the docs true** in the same PR, and note user-visible changes under *Unreleased* in
  [CHANGELOG.md](CHANGELOG.md).
- **Keep it local and private.** No accounts, telemetry or hosted services; secrets never go into
  workspace files, bundles, logs or agent responses.

## Pull requests
- Give the PR a clear title; it becomes the line in the release notes.
- Fill in the template: what changed, why, and how you tested it. Add screenshots for UI changes.
- CI must pass. A maintainer reviews every PR; PRs are squash- or rebase-merged into `main`.
- Draft PRs are welcome for early feedback.

## Releases
Maintainers cut releases by tagging `main`; CI builds the installers for Windows, macOS and Linux.
See [docs/releasing.md](docs/releasing.md).

## License
TwinBase is licensed under the [Apache License 2.0](LICENSE). Unless you explicitly state otherwise,
any contribution intentionally submitted for inclusion in TwinBase by you, as defined in the
Apache-2.0 license, shall be licensed as above, without any additional terms or conditions.
