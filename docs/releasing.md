# Releasing

Installers are built by `.github/workflows/ci.yml`, only for a release (or a manual dry run).
Pull requests and pushes to `main` run the checks only.

## What a release contains

| File | For |
| --- | --- |
| `TwinBase-Windows-Setup-x64.exe` | Windows 10/11 installer (NSIS) |
| `TwinBase-Windows-Portable-x64.zip` | Windows without installing (`twinbase-desktop.exe` is the app, `twinbase.exe` the CLI) |
| `TwinBase-macOS-universal.dmg` | macOS 11+, Apple silicon and Intel in one app |
| `TwinBase-Linux-amd64.deb`, `TwinBase-Linux-x86_64.rpm`, `TwinBase-Linux-x86_64.AppImage` | Linux x86_64 (built on Ubuntu 22.04, so they run on newer distributions) |
| `twinbase-cli-*` | The command line alone: headless mocks for tests and CI, the MCP bridge |
| `SHA256SUMS.txt` | Checksums of all of the above |

Every desktop package contains the `twinbase` command line next to the app (the MCP stdio bridge
uses it). Each file has a [build provenance attestation](https://docs.github.com/actions/security-for-github-actions/using-artifact-attestations):

```sh
gh attestation verify TwinBase-macOS-universal.dmg --repo LibreGuild/twinbase
```

The builds are not code-signed yet (macOS apps are signed ad hoc); the release notes tell users how
to open them.

## Cutting a release

1. Make sure `main` is green and `CHANGELOG.md` has the release's notes.
2. Set the version in the three places that must agree (CI refuses a mismatch):
   `Cargo.toml` (`[workspace.package] version`), `apps/desktop/package.json` and
   `apps/desktop/src-tauri/tauri.conf.json`. Merge that through a pull request.
3. Tag the merge commit and push the tag (only maintainers can create `v*` tags):

   ```sh
   git tag -a v0.2.0 -m "TwinBase 0.2.0"
   git push origin v0.2.0
   ```

4. CI runs the checks, creates a draft release, builds Windows, macOS (universal) and Linux in
   parallel, uploads the files with attestations, writes `SHA256SUMS.txt` and the notes (download
   table plus the pull requests merged since the previous release, grouped by label through
   `.github/release.yml`), and publishes the release. If any build fails, the draft is deleted
   and nothing is published; fix the problem, delete the tag and tag again.

## Trying the packaging without releasing

Run the **CI** workflow by hand (Actions → CI → Run workflow, or `gh workflow run ci.yml --ref main`).
It builds every package and keeps them as artifacts of that run for 14 days; nothing is released.

## Building locally

```sh
cd apps/desktop
npm ci
npm run package                                              # this system's installers
npm run package -- --target universal-apple-darwin --bundles app,dmg   # macOS universal
```

`npm run package` builds the `twinbase` CLI for the target first and places it where Tauri
expects its external binary (`src-tauri/tauri.bundle.conf.json`); plain `cargo build` and
`tauri dev` don't need it.

## Website

`site/` is a static page published to GitHub Pages (https://libreguild.github.io/twinbase/) by
`.github/workflows/pages.yml` whenever it changes on `main`. Enable it once in the repository
settings: **Pages → Source: GitHub Actions**. Screenshots live in `site/assets/shots/` and
`.github/assets/` (README).
