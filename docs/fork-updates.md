# MonoCode Fork updates

Install the appropriate macOS DMG from
[fork releases](https://github.com/EricRasputin/monocode-eric/releases/latest) in
`/Applications`. The app keeps the existing `com.monocode.fork.worktrees` identity
and saved conversations. Do not install upstream MonoCode as a fork update.

Choose **Check for Updates…** from the app menu or Settings. If a newer release
exists, the app shows its version and release notes and asks whether to install
and restart. Declining leaves the app running. Accepting downloads the package,
verifies its signature, installs it, and restarts. Startup checks also show an
available update in the sidebar. Failed checks or downloads do not count as a
successful update.

The old `0.1.x` fork builds have no update endpoint or public key, so they need
one initial replacement with a `0.2.x` build. Quit the old app first. If both
`~/Applications/MonoCode Fork.app` and `/Applications/MonoCode Fork.app` exist,
launch the new copy from `/Applications` to avoid opening the old build.

## Releases

Merges accumulate on `main`. The `CI` workflow runs checks for every push and PR,
but neither event packages or publishes an app update. You decide when the
accumulated changes are ready to ship:

1. Open [Actions → CI](https://github.com/EricRasputin/monocode-eric/actions/workflows/ci.yml).
2. Choose **Run workflow** and select the **main** branch.
3. Enable **Publish fork update after checks pass (main only)**, then run it.

That run checks the selected main commit on macOS, Linux, and Windows. Only after
**all** checks pass does it call `fork-release.yml` to package, sign, and publish
both macOS builds. Commits merged after the run starts wait for a later release.
The app offers the new version through **Check for Updates…** after publication.
No local build, version edit, tag push, or agent request is needed.

The publish option is off by default, so a manual run can also be used just to
check CI. Selecting another branch never publishes, even with the option enabled.

- The app's fork version is `0.2.<CI run number>`. Upstream's version stays in
  `package.json`, Cargo, and the base Tauri config; the fork config overrides it.
  Version numbers can skip because CI-only runs do not publish releases.
- Release tags use `fork-v0.2.<CI run number>`, so upstream `v*` tags stay separate.
- Both `darwin-aarch64` and `darwin-x86_64` packages must finish successfully.
- Packaging adds a changelog section for the fork version from changes on main
  since the previous fork release, so the post-update **What's new** view works.
  This generated section is bundled in the app without committing version bumps.
- The workflow checks bundle identity, version, architecture, and code signature.
  It stages a DMG, signed `.app.tar.gz`, and signature for each architecture.
- A single publisher validates artifact hashes and commit/version consistency,
  then uploads all packages, `SHA256SUMS`, and `latest.json` to a draft release.
  Only a complete upload becomes public and the latest release.
- The app reads `releases/latest/download/latest.json`; package URLs point to
  immutable version tags. The embedded public key verifies update signatures.
- Releases are serialized. Reruns and delayed older builds never replace a
  published newer release. A failed draft can be retried with the same CI run;
  a fresh manual CI run with publishing enabled gets a new version.

The inherited upstream release workflow is restricted to `hardbeat920/monocode`.
It cannot publish an upstream-branded release over this fork's update feed.
Fork distribution currently targets macOS; Linux and Windows remain CI targets.

## Signing

`src-tauri/tauri.fork.conf.json` contains the public updater key. The matching
private key is the GitHub Actions repository secret `FORK_UPDATER_PRIVATE_KEY`,
passed only to the packaging step. The key has no password, so the workflow sets
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` to an empty string. Never commit the private
key or print it in logs. The initial local backup is stored outside the repo at
`~/.config/monocode-eric/updater/private.key` with owner-only permissions; keep a
durable backup because losing it prevents updates to already-installed apps.

Tauri update signing authenticates downloaded update packages. Apple signing and
notarization are separate: these builds currently use the existing ad-hoc macOS
signature. A downloaded installer may require approval in macOS Privacy &
Security on first installation. Apple Developer ID distribution and notarization
can be configured later without replacing the updater key or application identity.

References: [Tauri updater](https://v2.tauri.app/plugin/updater/) and
[macOS signing](https://v2.tauri.app/distribute/sign/macos/).

## Validation and recovery

`npm run check:web` includes release metadata tests and the updater interaction
tests. CI additionally compiles and tests Rust on all three platforms. To recover
from a bad release, revert the source change and manually run CI with publishing
enabled to ship a higher fork version. Do not move an existing release tag or
replace a published archive.

Local builds default to `0.2.0`; release builds get their version through the
Tauri config override in CI. Local builds are useful for development but are not
published releases. A private-key-free local build can disable generation of
updater artifacts using the command in the README.
