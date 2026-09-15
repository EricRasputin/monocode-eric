
<p align="center">
  <img src="public/monocode.png" alt="MonoCode" width="88" />
</p>

<h1 align="center">MonoCode</h1>

<p align="center">
  <strong>A desktop UI for your coding agents.</strong>
</p>

<p align="center">
  <img width="1680" height="1050" alt="Screenshot 2026-09-04 at 06 34 00" src="https://github.com/user-attachments/assets/2cd4a6ec-eb1e-4b45-8627-a76442ea3874" />
</p>

Works with your subscriptions on Claude Code, Codex, Cursor, Grok Build, OpenCode, Pi, omp, and fx. If they’re installed and logged in, MonoCode can run them. Tabs are sessions. The composer is the input. MonoCode does not sell tokens.

## Install

**This fork (macOS):** download **MonoCode Fork** for Apple Silicon (`darwin-aarch64`)
or Intel (`darwin-x86_64`) from [fork releases](https://github.com/EricRasputin/monocode-eric/releases/latest)
and install it in `/Applications`. Then use **Check for Updates…** in the app menu
or Settings. The app asks before downloading, installing, and restarting.
Builds from before `0.2.0` need this one-time installation to enable updates.
See [fork updates](docs/fork-updates.md) for release and signing details.

The download links below are for upstream MonoCode, which is a separate app.

> Install and log in to at least one provider first:
>
> - [Claude Code](https://claude.com/product/claude-code) - `claude auth login`
> - [Codex](https://developers.openai.com/codex/cli) - `codex login`
> - [Cursor CLI](https://cursor.com/cli) - `agent login`
> - [Grok Build](https://docs.x.ai/build/overview) - `curl -fsSL https://x.ai/cli/install.sh | bash` then `grok login`
> - [OpenCode](https://opencode.ai) - `opencode auth login`
> - [Pi](https://pi.dev/) - `npm install -g @earendil-works/pi-coding-agent`
> - [omp](https://omp.sh) - `curl -fsSL https://omp.sh/install | sh`
> - [fx](https://fx.sh) - `curl -fsSL https://fx.sh/setup.sh | bash` then `fx login`

macOS (Apple Silicon): download [MonoCode.dmg](https://dl.usemono.dev/MonoCode.dmg), open it, drag MonoCode to Applications.

macOS (Intel): download [MonoCode_x64.dmg](https://dl.usemono.dev/MonoCode_x64.dmg), open it, drag MonoCode to Applications.

Linux (x86_64): download the `.deb` or AppImage from [GitHub Releases](https://github.com/hardbeat920/monocode/releases/latest). Install the `.deb` with `sudo apt install ./MonoCode_*.deb`, or make the AppImage executable with `chmod +x MonoCode_*.AppImage` and run it directly.

Windows (x86_64): download the NSIS installer from [GitHub Releases](https://github.com/hardbeat920/monocode/releases/latest) and run it.

## Worktrees in this fork

Choose **New worktree** or **Current checkout** above the message box. For a new worktree, choose the starting branch and send; Monocode creates the checkout for that conversation. **Settings → Worktrees** defaults to manual cleanup review. You can explicitly save automatic retirement after the last conversation is archived; eligible checkouts are removed with code recovery, selected configuration, branches and conversation history preserved. See [the worktree guide](docs/worktrees.md).

## Some notes

This is very early and you should expect bugs.

Small, focused pull requests are welcome. Anything large is worth an issue first - see [CONTRIBUTING.md](CONTRIBUTING.md).

## Build from source

Changes to `main` run CI without publishing a release. When a batch is ready,
open **Actions → CI → Run workflow**, select **main**, and enable **Publish fork
update after checks pass (main only)**. After macOS, Linux, and Windows checks
pass, packaging and publishing run automatically. Users update from inside the
app; no local build is needed. The fork has its own versions (`1.0.0`, `1.0.1`,
and so on) and `fork-v*` tags, independent of upstream and CI run numbers. Leave
**Fork version** blank for the next patch or supply a newer minor/major version.
The app displays both versions, for example **1.0.0 (0.1.46)**: our fork release,
then the upstream release it is based on. `upstream-release.json` pins that base
to its release tag and commit.
See [fork updates](docs/fork-updates.md) for details.

For a local macOS build, run `npm ci` followed by `npm run build:fork` with
`TAURI_SIGNING_PRIVATE_KEY` set to the fork's signing key path and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` set to an empty string. The app, DMG, and
signed update archive are written under `target/release/bundle/`. This preserves
the **MonoCode Fork** identity (`com.monocode.fork.worktrees`) and its session data,
and checks only this fork's update feed. Without the signing key, use
`npm run build:fork -- --config '{"bundle":{"createUpdaterArtifacts":false}}'`
to create a local app and DMG without a publishable update archive.

When building from a session inside MonoCode Fork, leave the running app in
place. Quit it before replacing `/Applications/MonoCode Fork.app` with the
new bundle, then reopen it. Build in this checkout's own `target` directory so
other running development builds are unaffected.

Supports macOS, Linux, and Windows.

Need Node.js 20+ and a current stable Rust toolchain. On Linux, ensure standard Tauri prerequisites are installed (e.g. `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libsoup-3.0-dev`, `libjavascriptcoregtk-4.1-dev`). On Windows, the installer bootstraps the [WebView2](https://developer.microsoft.com/microsoft-edge/webview2/) runtime when it is missing.

```bash
npm install
npm run tauri dev
```

### Ubuntu / Debian packages

On an Ubuntu/Debian workstation, the repository can install the native Tauri prerequisites and build distributable Linux packages directly:

```bash
npm run setup:linux:deb
npm ci
npm run build:linux
```

The Linux build emits `.deb` and AppImage bundles under `target/release/bundle/`.
Tauri loads `src-tauri/tauri.linux.conf.json` automatically for Linux development and builds.

### Windows packages

```bash
npm ci
npm run build:windows
```

The Windows build emits an NSIS installer under `target/release/bundle/nsis/`.
Tauri loads `src-tauri/tauri.windows.conf.json` automatically for Windows development and builds.

## License

[MIT](LICENSE). Provider names and logos are trademarks of their owners - see [NOTICE](NOTICE).
