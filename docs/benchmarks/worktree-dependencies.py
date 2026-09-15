#!/usr/bin/env python3
"""Bounded, disposable #18 benchmark. No project/global configuration changes.

Run with python3 docs/benchmarks/worktree-dependencies.py. Retains its unique
temporary monocode-sharing-* directory, exact logs, lockfiles and results.json.
Only generated outputs inside that directory are deleted. No Git worktrees.
"""

import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time


ROOT = Path(tempfile.mkdtemp(prefix="monocode-sharing-")).resolve()
LOGS = ROOT / "logs"
LOGS.mkdir()
START = time.monotonic()
RESULTS = {"root": str(ROOT), "measurements": [], "snapshots": {}}
ENV = os.environ.copy()
for key in list(ENV):
    if key.startswith(("npm_config_", "NPM_CONFIG_", "CARGO_", "RUSTC_")):
        ENV.pop(key)
empty = ROOT / "empty-config"
empty.touch()
ENV.update({
    "npm_config_userconfig": str(empty),
    "npm_config_globalconfig": str(ROOT / "empty-global-config"),
    "npm_config_registry": "https://registry.npmjs.org",
    "npm_config_audit": "false", "npm_config_fund": "false",
    "npm_config_update_notifier": "false",
    "COREPACK_HOME": str(ROOT / "tools"),
    "COREPACK_ENABLE_AUTO_PIN": "0", "COREPACK_ENABLE_PROJECT_SPEC": "0",
    "XDG_CONFIG_HOME": str(ROOT / "config"),
    "XDG_CACHE_HOME": str(ROOT / "metadata"),
    "XDG_DATA_HOME": str(ROOT / "data"),
    "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": "",
    "RUSTFLAGS": "", "CARGO_BUILD_JOBS": "2", "CI": "true",
})
PNPM = ["corepack", "pnpm@10.32.1"]


def allocated(paths):
    """lstat blocks, no symlink traversal, deduplicate device/inode globally."""
    seen, total = set(), 0
    for path in paths:
        if not path.exists():
            continue
        for parent, dirs, files in os.walk(path, followlinks=False):
            for entry in [Path(parent)] + [Path(parent) / x for x in files]:
                stat = entry.lstat()
                identity = (stat.st_dev, stat.st_ino)
                if identity not in seen:
                    seen.add(identity)
                    total += stat.st_blocks * 512
            for name in dirs:
                entry = Path(parent) / name
                if entry.is_symlink():
                    stat = entry.lstat()
                    identity = (stat.st_dev, stat.st_ino)
                    if identity not in seen:
                        seen.add(identity)
                        total += stat.st_blocks * 512
    return total


def persist():
    (ROOT / "results.json").write_text(json.dumps(RESULTS, indent=2) + "\n")


def run(label, command, cwd=ROOT, env=None):
    if time.monotonic() - START > 900:
        raise RuntimeError("15-minute total benchmark bound reached")
    started = time.monotonic()
    try:
        process = subprocess.run(command, cwd=cwd, env=env or ENV,
                                 capture_output=True, text=True, timeout=180)
        output, code = process.stdout + process.stderr, process.returncode
    except subprocess.TimeoutExpired as error:
        output, code = str(error), "timeout"
    item = {"label": label, "command": command, "cwd": str(cwd),
            "seconds": round(time.monotonic() - started, 6), "exit": code}
    (LOGS / (label + ".log")).write_text(json.dumps(item) + "\n" + output)
    RESULTS["measurements"].append(item)
    persist()
    print(json.dumps(item), flush=True)
    if code != 0:
        raise RuntimeError(f"{label}: {code}; see retained log")
    return output.strip()


def snapshot(label, components):
    RESULTS["snapshots"][label] = {
        "components": {key: allocated(paths) for key, paths in components.items()},
        "union_bytes": allocated([p for paths in components.values() for p in paths]),
    }
    persist()
    if allocated([ROOT]) > 2 * 1024**3:
        raise RuntimeError("2 GiB retained-fixture bound exceeded")


def remove(path):
    assert ROOT in path.resolve().parents and not path.is_symlink()
    if path.exists():
        shutil.rmtree(path)


def repo(path):
    run(path.name + "-git-init", ["git", "init", "-b", "main", str(path)])
    run(path.name + "-git-add", ["git", "add", "."], path)
    run(path.name + "-git-commit", ["git", "-c", "user.name=Fixture",
        "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false",
        "commit", "-m", "Disposable dependency benchmark"], path)


def parallel(label, tasks):
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        futures = [pool.submit(run, *task) for task in tasks]
        for future in futures:
            future.result()
    RESULTS[label + "_wall_seconds"] = round(time.monotonic() - started, 6)
    persist()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def node():
    seed = ROOT / "node-seed"
    (seed / "src").mkdir(parents=True)
    (seed / "package.json").write_text(json.dumps({
        "name": "sharing-fixture", "private": True, "version": "1.0.0",
        "scripts": {"build": "tsc && esbuild lib/main.js --bundle --platform=node --outfile=dist/main.cjs"},
        "dependencies": {"typescript": "5.8.3", "esbuild": "0.25.5", "lodash": "4.17.21"},
    }))
    (seed / "tsconfig.json").write_text(json.dumps({"compilerOptions": {
        "target": "ES2022", "module": "commonjs", "outDir": "lib",
        "strict": True, "skipLibCheck": True}, "include": ["src"]}))
    (seed / ".gitignore").write_text("node_modules/\nlib/\ndist/\n")
    imports, calls = [], []
    for i in range(250):
        (seed / "src" / f"m{i}.ts").write_text(
            f"export const f{i} = (x: number): number => x * {i + 1};\n")
        imports.append(f"import {{ f{i} }} from './m{i}';")
        calls.append(f"f{i}(2)")
    (seed / "src" / "main.ts").write_text("\n".join(imports) +
        "\nconsole.log(" + "+".join(calls) + ");\n")
    seed_env = {**ENV, "npm_config_cache": str(ROOT / "seed-npm-cache")}
    run("node-lock-npm", ["npm", "install", "--package-lock-only", "--ignore-scripts"], seed, seed_env)
    run("node-lock-pnpm", PNPM + ["install", "--lockfile-only", "--ignore-scripts",
        "--store-dir", str(ROOT / "seed-pnpm-store")], seed, seed_env)
    for method in ("npm", "clone", "hardlink"):
        label = "node-" + method
        cache = ROOT / (label + "-cache")
        roots = [ROOT / (label + "-" + suffix) for suffix in ("a", "b")]
        env = {**ENV, "npm_config_cache": str(cache if method == "npm" else ROOT / "runner-npm-cache")}
        for path in roots:
            shutil.copytree(seed, path)
            if method == "npm":
                (path / "pnpm-lock.yaml").unlink()
            else:
                (path / "package-lock.json").unlink()
            repo(path)
        install = (["npm", "ci", "--ignore-scripts"] if method == "npm" else
                   PNPM + ["install", "--frozen-lockfile", "--ignore-scripts", "--store-dir", str(cache),
                           "--package-import-method", method])
        build = ["npm", "run", "build"]
        components = {"a": [roots[0]], "b": [roots[1]], "store": [cache]}
        run(label + "-cold-prepare", install, roots[0], env)
        run(label + "-cold-build", build, roots[0], env)
        run(label + "-warm-prepare", install + ["--offline"], roots[1], env)
        run(label + "-warm-fresh-build", build, roots[1], env)
        run(label + "-warm-existing-build", build, roots[0], env)
        for path in roots:
            assert run(path.name + "-execute", ["node", "dist/main.cjs"], path, env) == "62750"
        snapshot(label + "-two-built", components)
        for path in roots:
            for name in ("node_modules", "lib", "dist"):
                remove(path / name)
        parallel(label + "-concurrent-prepare", [
            (path.name + "-parallel-prepare", install + ["--offline"], path, env) for path in roots])
        parallel(label + "-concurrent-build", [
            (path.name + "-parallel-build", build, path, env) for path in roots])
        assert digest(roots[0] / "dist/main.cjs") == digest(roots[1] / "dist/main.cjs")
        snapshot(label + "-parallel-built", components)
        source_hash = digest(roots[0] / "src/main.ts")
        for name in ("node_modules", "lib", "dist"):
            remove(roots[0] / name)
        assert digest(roots[0] / "src/main.ts") == source_hash
        assert run(label + "-survivor", ["node", "dist/main.cjs"], roots[1], env) == "62750"
        snapshot(label + "-removed-a", components)
        for name in ("node_modules", "lib", "dist"):
            remove(roots[1] / name)
        snapshot(label + "-removed-both", components)
        prune = (["npm", "cache", "clean", "--force"] if method == "npm" else
                 PNPM + ["store", "prune", "--store-dir", str(cache)])
        run(label + "-prune", prune, roots[0], env)
        snapshot(label + "-pruned", components)


def rust():
    seed = ROOT / "rust-seed"
    (seed / "src").mkdir(parents=True)
    (seed / "Cargo.toml").write_text('''[package]
name = "sharing-fixture"
version = "0.1.0"
edition = "2021"
[dependencies]
regex = "=1.11.1"
serde_json = "=1.0.140"
sha2 = "=0.10.9"
''')
    (seed / "src/main.rs").write_text('''use sha2::{Digest, Sha256};
fn main() {
    let re = regex::Regex::new(r"[a-z]+").unwrap();
    let words: Vec<_> = re.find_iter("bounded fixture build").map(|m| m.as_str()).collect();
    let json = serde_json::to_string(&words).unwrap();
    println!("{:x}", Sha256::digest(json.as_bytes()));
}
''')
    (seed / ".gitignore").write_text("target/\n")
    run("rust-lock", ["cargo", "generate-lockfile"], seed,
        {**ENV, "CARGO_HOME": str(ROOT / "seed-cargo-home")})
    for method in ("private-sources", "shared-sources"):
        label = "rust-" + method
        roots = [ROOT / (label + "-" + suffix) for suffix in ("a", "b")]
        homes = [ROOT / (label + "-home-" + suffix) for suffix in ("a", "b")]
        if method == "shared-sources":
            homes[1] = homes[0]
        envs = [{**ENV, "CARGO_HOME": str(home)} for home in homes]
        for path in roots:
            shutil.copytree(seed, path)
            repo(path)
        components = {"a": [roots[0]], "b": [roots[1]], "cargo_homes": homes}
        for i, phase in enumerate(("cold", "second")):
            run(label + "-" + phase + "-prepare", ["cargo", "fetch", "--locked"], roots[i], envs[i])
            run(label + "-" + phase + "-build", ["cargo", "build", "--locked", "--offline"], roots[i], envs[i])
        run(label + "-warm-existing-build", ["cargo", "build", "--locked", "--offline"], roots[0], envs[0])
        snapshot(label + "-two-built", components)
        for path, env in zip(roots, envs):
            run(path.name + "-clean", ["cargo", "clean"], path, env)
        parallel(label + "-concurrent-prepare", [(path.name + "-parallel-prepare",
            ["cargo", "fetch", "--locked", "--offline"], path, env) for path, env in zip(roots, envs)])
        parallel(label + "-concurrent-build", [(path.name + "-parallel-build",
            ["cargo", "build", "--locked", "--offline"], path, env) for path, env in zip(roots, envs)])
        outputs = [run(path.name + "-execute", [str(path / "target/debug/sharing-fixture")], path, env)
                   for path, env in zip(roots, envs)]
        assert outputs[0] == outputs[1]
        snapshot(label + "-parallel-built", components)
        for path, env in zip(roots, envs):
            source_hash = digest(path / "src/main.rs")
            run(path.name + "-final-clean", ["cargo", "clean"], path, env)
            assert digest(path / "src/main.rs") == source_hash
            snapshot(path.name + "-cleaned", components)
        for home in set(homes):
            remove(home)
        snapshot(label + "-evicted", components)


print(ROOT, flush=True)
try:
    sccache = shutil.which("sccache")
    RESULTS["sccache"] = {
        "available": sccache is not None,
        "executable": sccache,
        "measured": False,
        "reason": ("Executable present but compiler-cache benchmark not implemented by this runner; timing, hits, size and eviction unavailable"
                   if sccache else "sccache executable absent from PATH; compiler-cache timing, hits, size and eviction unavailable"),
    }
    if sccache:
        RESULTS["sccache"]["version"] = run("version-sccache", [sccache, "--version"])
    RESULTS["versions"] = {tool: run("version-" + tool, command) for tool, command in {
        "node": ["node", "--version"], "npm": ["npm", "--version"],
        "corepack": ["corepack", "--version"], "pnpm": PNPM + ["--version"],
        "rustc": ["rustc", "-Vv"], "cargo": ["cargo", "-V"],
        "git": ["git", "--version"], "macos": ["sw_vers"],
    }.items()}
    node()
    rust()
    snapshot("retained-overhead", {"tools": [ROOT / "tools"],
        "metadata": [ROOT / "metadata", ROOT / "data", ROOT / "runner-npm-cache"],
        "seed-caches": [ROOT / "seed-cargo-home", ROOT / "seed-npm-cache", ROOT / "seed-pnpm-store"]})
    RESULTS["completed"] = True
except Exception as error:
    RESULTS["error"] = str(error)
    raise
finally:
    RESULTS["elapsed_seconds"] = round(time.monotonic() - START, 6)
    persist()
