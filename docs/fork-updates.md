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

Update checks allow up to five seconds to establish a connection and fifteen
seconds to retrieve the feed, including redirects. This lets the HTTP client
move past an unreachable CDN address without waiting for the operating system's
TCP timeout. A timed-out check reports an error and can be retried. Downloads
keep the connection limit but have no fifteen-second total limit.

The old `0.1.x` fork builds have no update endpoint or public key, so they need
one initial replacement with a `0.2.x` or newer build. Quit the old app first. If both
`~/Applications/MonoCode Fork.app` and `/Applications/MonoCode Fork.app` exist,
launch the new copy from `/Applications` to avoid opening the old build.

## Version display and upstream base

Settings and the native About window show the fork version followed by its
upstream base, for example **1.0.0 (0.1.46)**. The first number is our release;
the number in parentheses is the upstream release whose source we incorporated.
The About window and Settings also label that relationship explicitly.

A fork-only change can ship as `1.0.1 (0.1.46)`. After incorporating upstream
`v0.1.47`, a later fork release could be `1.0.2 (0.1.47)`. Updating the upstream
base does not reset the fork's version sequence. The updater still compares the
plain fork version, such as `1.0.2`.

`upstream-release.json` records the upstream repository, release tag, version,
and exact commit. Update this record in the same PR that incorporates an upstream
release. It is bundled into the app, so the installed app continues to report
its actual base even after newer upstream releases become available. It never
fetches the latest upstream version to construct this label.

Packaging verifies that the source version matches the record, the recorded tag
in the upstream repository points to its commit, and that commit is an ancestor
of the build. Both macOS
packages must report the same upstream metadata. The GitHub release title uses
the paired display, and `latest.json` includes the upstream record alongside the
plain fork `version`.

## Releases

Merges accumulate on `main`. The `CI` workflow runs checks for every push and PR,
but neither event packages or publishes an app update. You decide when the
accumulated changes are ready to ship:

1. Open [Actions → CI](https://github.com/EricRasputin/monocode-eric/actions/workflows/ci.yml).
2. Choose **Run workflow** and select the **main** branch.
3. Enable **Publish fork update after checks pass (main only)**.
4. Leave **Fork version** blank for the next patch release, or enter a version
   such as `1.1.0` or `2.0.0` when deliberately starting a minor or major release.
5. Run the workflow.

That run checks the selected main commit on macOS, Linux, and Windows. Only after
**all** checks pass does it call `fork-release.yml` to package, sign, and publish
both macOS builds. Commits merged after the run starts wait for a later release.
The app offers the new version through **Check for Updates…** after publication.
No local build, version edit, tag push, or agent request is needed.

The publish option is off by default, so a manual run can also be used just to
check CI. Selecting another branch never publishes, even with the option enabled.

- The fork has its own version sequence: `1.0.0`, `1.0.1`, `1.0.2`, and so on.
  CI run numbers and upstream releases do not advance it. The first release under
  this scheme is `1.0.0`, which upgrades from the earlier `0.2.x` fork builds.
  Only stable versions in `major.minor.patch` form are accepted.
- Version selection runs inside the serialized release workflow, before either
  architecture builds. It finds the highest published stable `fork-v*` version
  and increments its patch number unless you supply a newer version explicitly.
  Drafts, prereleases, and upstream tags do not determine the next version.
- Release tags use `fork-v1.0.0`, independently of upstream's `v*` tags. The fork
  also has its own application identity, updater key, and release feed, so an
  upstream release with the same numeric version cannot be installed as an update.
- Upstream's version stays in `package.json`, Cargo, and the base Tauri config;
  the fork config overrides the app version. Release notes retain the upstream
  version as provenance rather than using it as the fork's version.
- Both `darwin-aarch64` and `darwin-x86_64` packages must finish successfully.
- Packaging adds a changelog section for the fork version from all fork commits
  since the previous fork release, including commits brought in through merges.
  The post-update **What's new** view and GitHub release notes list these changes.
  Upstream changes stay in the bundled upstream changelog.
  This generated section is bundled in the app without committing version bumps.
- The workflow checks bundle identity, version, architecture, and code signature.
  It stages a DMG, signed `.app.tar.gz`, and signature for each architecture.
- A single publisher validates artifact hashes and commit/version consistency,
  then uploads all packages, `SHA256SUMS`, and `latest.json` to a draft release.
  Only a complete upload becomes public and the latest release.
- The app reads `releases/latest/download/latest.json`; package URLs point to
  immutable version tags. The embedded public key verifies update signatures.
- An automatic version selection for an already-released commit skips packaging.
  A delayed run for a commit that does not include the latest fork release fails
  before packaging. Explicit versions must be newer than the latest release.
  Neither reruns nor delayed builds replace a published newer release.
- A failed draft can be retried for its original commit and version. If that
  version has a draft for another commit, choose a different explicit fork version;
  the workflow refuses to overwrite a draft belonging to another commit.

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

Local builds default to `1.0.0`; release builds get their version through the
Tauri config override in CI. Local builds are useful for development but are not
published releases. A private-key-free local build can disable generation of
updater artifacts using the command in the README.
