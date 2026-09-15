# Dependency sharing investigation (#18)

Measured on 2026-09-15 after the four #13 implementation tasks, based on
`bd813b7`. **Retain this project's npm installs and separate build outputs.**
The small fixtures show a substantial accounting reduction from pnpm hard links,
but no installation-speed advantage over npm here. Hard-link mutation constraints
make that insufficient evidence for a project migration. Ordinary Cargo already
shares downloaded sources; it still compiles into each checkout's own `target`.
Compiler-cache measurements are unavailable because `sccache` is not installed.

## Reproduction and scope

Run `python3 docs/benchmarks/worktree-dependencies.py` from the checkout. The
[runner](benchmarks/worktree-dependencies.py) creates ordinary disposable Git
repositories in the system temporary directory, not agent worktrees. It retains
lockfiles, exact command output and `results.json`. The final run took **78.275 s**,
with a 180-second timeout per subprocess, a 15-minute dispatch deadline, two
concurrent builds at most, and a 2 GiB allocated-size guard at measurement
boundaries. These are experiment bounds, not filesystem quotas. No project's
package manager, global tool configuration, or user worktree was changed.

- [Raw final measurements](benchmarks/worktree-dependencies-2026-09-15.json).
- Final stdout log: `/tmp/monocode-18-benchmark-final.log`.
- Exact logs and fixture manifests/lockfiles:
  `/tmp/monocode-18-benchmark-evidence.tar.gz` and
  `/private/var/folders/m2/kvd1y03j1fxc_ym_gc6jkfbc0000gn/T/monocode-sharing-oh9emqc4/`.
- Initial run, before correcting executable detection in the runner:
  `/tmp/monocode-18-benchmark-run.log`, with its original temporary directory
  recorded on the first line. The tables below use only the final run.

| Tool/platform | Measured version |
| --- | --- |
| macOS / filesystem / CPU architecture | 27.0, build 26A428 / APFS / arm64 |
| Node / npm | v24.13.1 / 11.14.1 |
| Corepack / fixture-only pnpm | 0.34.6 / 10.32.1 |
| rustc | 1.98.1 (`48a229cea`, 2026-09-01), LLVM 22.1.8 |
| Cargo / target | 1.98.1 (`797e8a9bc`) / aarch64-apple-darwin |
| Git | 2.54.0 (Apple Git-157) |
| sccache | Unavailable: no executable on `PATH` |

The runner detects `sccache` with `shutil.which`. If installed on another machine,
it records the executable/version and explicitly says the compiler-cache
benchmark is **not implemented**, rather than falsely calling it absent.

### Fixtures and procedure

Node: 251 TypeScript source files, TypeScript 5.8.3, esbuild 0.25.5 and lodash
4.17.21. `tsc` emits private `lib/`; esbuild bundles into private `dist/`. Each
bundle must execute and print `62750`; concurrent bundle hashes must match.
Lodash contributes installed package volume; the generated calculation does not
exercise its API. esbuild's prebuilt macOS executable runs successfully. This is
a small dependency/build fixture, not a benchmark of the entire desktop app.

Rust: a CLI using regex 1.11.1, serde_json 1.0.140 and sha2 0.10.9, with the
resolved transitive graph retained in `Cargo.lock`. It serializes matched words
and prints their SHA-256. Both concurrent binaries must produce identical output.
Cargo uses the development profile, normal incremental defaults and two build
jobs per process. Every checkout has a distinct `target`; compiler wrappers are
cleared for the baseline.

Lockfile generation and package-manager bootstrap occur separately from timed
preparation, using separate seed caches. Each strategy begins with an empty
package/source store and two fresh repositories. A is prepared and built first;
B then prepares against the same store (except the deliberately private Cargo
home control). Both installations use frozen/locked graphs. Node dependency
lifecycle scripts are disabled with `--ignore-scripts`; `npm run build` explicitly
runs the fixture build. Warm Node preparation is forced offline. Rust builds are
offline after `cargo fetch --locked`.

“Cold” means empty **experiment** store and outputs. It does not mean flushed OS,
DNS, registry/CDN or compiler executable caches. “Warm fresh” means a new checkout
with a populated package/source store but no build outputs. “Warm existing” reruns
the build with its existing outputs. Node has no incremental TypeScript cache in
this fixture, so its warmer timings do not demonstrate compiler-cache hits.
Concurrent runs delete both output trees first, install/fetch concurrently with
warm caches, then build concurrently. The app was idle during the final benchmark;
native lifecycle actions and repository checks ran afterward.

One observation per phase is reported, with no claim of statistical significance.
Subsecond differences and network-dependent cold results should not be used to
predict this project's build times. Lockfile/toolchain changes, native addon
compilation, remote caches and cross-volume imports were not benchmarked.

## Measured elapsed seconds

### Node

| Store/import | Cold preparation A | Cold build A | Warm preparation B | Warm fresh build B | Warm existing build A | Two concurrent preparations, wall | Two concurrent fresh builds, wall |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| npm cache, private installs | 0.510 | 0.961 | 0.399 | 0.885 | 0.530 | 0.562 | 0.928 |
| pnpm store, clone imports | 0.962 | 1.398 | 0.457 | 1.417 | 0.607 | 0.542 | 0.969 |
| pnpm store, hard-link imports | 1.027 | 1.371 | 0.714 | 1.211 | 0.567 | 0.851 | 0.870 |

All installs/builds exited zero. The two npm concurrent builds took 0.927/0.927 s;
pnpm clone 0.967/0.968 s; pnpm hard link 0.868/0.869 s. No build-output lock or
corruption was observed. The package store did not cache `lib/` or `dist/`.

### Rust

| Source cache / private targets | Cold fetch A | Cold build A | Fetch B | Fresh build B | Warm existing build A | Two concurrent fetches, wall | Two concurrent clean builds, wall |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Separate Cargo homes (control) | 0.656 | 6.852 | 0.651 (cold) | 6.897 | 0.145 | 0.105 | 7.921 |
| Shared Cargo home (ordinary Cargo behavior) | 0.652 | 6.790 | 0.172 (warm) | 6.061 | 0.138 | 0.103 | 7.462 |
| Shared sccache + private targets | unavailable | unavailable | unavailable | unavailable | unavailable | unavailable | unavailable |

Both shared-source concurrent builds passed (7.461/7.410 s). Their logs include
short waits for Cargo's **package-cache** lock, followed by each compiling its own
dependencies. There was no shared output directory or target-directory lock.
The private-source control took 7.920/7.488 s. Warm existing Cargo builds were
effectively no-ops; that large speed difference comes from retaining `target`,
not merely sharing downloaded sources. The source-store comparison demonstrates
avoided fetching and source duplication; it does not establish compiler reuse.

## Allocated bytes, including stores

Measurement uses `lstat().st_blocks * 512`, does not follow symlinks, and counts
each `(device, inode)` once across both repositories and all listed stores.
Repository totals include source, Git metadata, lockfiles and generated outputs.
Component totals can overlap for hard links; **use the union, not their sum**.

| Strategy, two completed builds | A bytes | B bytes | Shared/store bytes | Union bytes |
| --- | ---: | ---: | ---: | ---: |
| npm private installs | 40,640,512 | 40,640,512 | 8,806,400 | 90,087,424 |
| pnpm clones | 40,665,088 | 40,665,088 | 38,420,480 | 119,750,656 |
| pnpm hard links | 40,591,360 | 40,591,360 | 38,420,480 | 43,515,904 |
| Rust, private Cargo homes | 124,653,568 | 124,653,568 | 57,131,008 (two homes) | 306,438,144 |
| Rust, shared Cargo home | 124,653,568 | 124,653,568 | 28,565,504 | 277,872,640 |

The Node hard-link union is **46,571,520 bytes (51.7%) smaller** than npm including
its cache. That is a measured reduction in this accounting model, not an observed
filesystem free-space delta. It requires package files to remain immutable. The
Rust source cache saves **28,565,504 bytes (9.3%)** against unnecessarily private
Cargo homes; it is already the normal Cargo arrangement. Each Rust target still
accounts for 124,502,016 bytes after excluding retained source/Git metadata.

**APFS physical clone savings are unavailable.** Different inodes may share
copy-on-write extents; inode-deduplicated `st_blocks` cannot discover those extents.
The clone row therefore does not prove that clones physically use more space than
npm or less than hard links. Neither these totals nor deletion estimates imply
that APFS snapshots immediately release the displayed number of bytes.

Separately retained measurement/bootstrap overhead was 21,884,928 bytes for
fixture-only Corepack/pnpm, 32,768 bytes for auxiliary metadata/runner logs, and
24,416,256 bytes for seed caches: **46,333,952 bytes** total. These are outside the
strategy rows and are explicitly additional experiment disk usage. Existing Node,
npm and Rust installations are also outside the rows. Temporary repository seeds,
the runner's evidence logs and its evidence archive are harness overhead, not
reusable dependency/build stores. No global store size is attributed to a fixture.

## Cleanup and eviction

These are union bytes after the concurrent phase; npm's cache grows slightly
because of additional command logs. Sources and Git repositories remain.

| Strategy | Two built | A outputs cleared | Both outputs cleared | Both cleared and experiment store evicted |
| --- | ---: | ---: | ---: | ---: |
| npm | 90,099,712 | 50,708,480 | 11,317,248 | 2,535,424 |
| pnpm clones | 119,750,656 | 80,330,752 | 40,910,848 | 2,490,368 |
| pnpm hard links | 43,515,904 | 42,213,376 | 40,910,848 | 2,490,368 |
| Rust private sources | 306,438,144 | 181,936,128 | 57,434,112 | 303,104 |
| Rust shared sources | 277,872,640 | 153,370,624 | 28,868,608 | 303,104 |

After clearing Node A, B's bundle still ran correctly and A's source hash was
unchanged. Clearing hard-linked installs frees few blocks while the store retains
their inodes. `npm cache clean --force` against only the experiment cache took
0.196 s and left 36,864 bytes of metadata/logs. `pnpm store prune` took 0.242 s
(clone) / 0.247 s (hard link), reducing each store to zero measured blocks after
both installs were removed. Rust used `cargo clean` per checkout, verified source
hashes, then removed only the experiment Cargo homes; source cache eviction is
not performed by `cargo clean`. No cache was evicted during a build.

npm's content-addressed cache verifies downloaded data; it does not deduplicate
the installed `node_modules` trees. It may grow across package versions, and
`npm cache verify` can collect unneeded entries. A cache is disposable, not an
offline availability guarantee. [npm cache documentation](https://docs.npmjs.com/cli/v11/commands/npm-cache/)

pnpm pruning removes unreferenced packages; a later checkout that needs an evicted
version may need the network again. The experiment measured explicit pruning,
not timed retention or automatic eviction. [pnpm 10 store documentation](https://pnpm.io/10.x/cli/store)

Cargo keeps registry archives and extracted sources in its home. Current Cargo
automatically cleans unused global source-cache entries during substantial online
commands, normally checking daily; offline runs suppress this cleanup. That policy
does not clean build artifacts. The short fixture did not age entries enough to
measure automatic eviction. [Cargo home](https://doc.rust-lang.org/cargo/guide/cargo-home.html),
[Cargo cache configuration](https://doc.rust-lang.org/cargo/reference/config.html#cache)

## Compatibility and failure constraints

| Boundary | Implication |
| --- | --- |
| Lockfiles and dependency graph | Keep the lockfile and package-manager version per project. `npm ci` rejects a mismatched manifest/lock and replaces its install tree. pnpm layout/peer resolution can expose undeclared dependencies; a package-manager migration needs project tests. |
| Package-file mutation | pnpm clones support independent writes. Hard-linked package bytes are shared: tools that patch them in place can affect another checkout and its store. Neither installs nor their virtual stores should become a common mutable directory. |
| Filesystems/volumes | pnpm imports prefer cloning when supported, with link/copy fallbacks in auto mode. Hard links require the same filesystem; cross-volume stores copy. Only APFS on one volume was exercised. |
| Native modules and scripts | The fixture exercised an esbuild prebuilt executable, not node-gyp/addon compilation or install lifecycle scripts. Node ABI, OS/architecture, libc, toolchain and approved build scripts require validation before reuse of native artifacts. |
| Rust configuration | Lockfile, rustc version, target, profile, features, flags and build-script inputs can alter artifacts and cache hits. Independent `target` directories retain normal Cargo fingerprints and mutable incremental/link outputs. |
| Compiler caches | sccache can share supported compiler results with private targets. Rust incremental compilation must be disabled; linker-invoking crate types are not cached, and filesystem-reading proc macros have caveats. Cache hits, concurrency, footprint and eviction remain unmeasured here. |
| Isolation and capacity accounting | Share stores only among mutually trusted jobs. Disk admission currently counts managed checkout roots, not an external cache as a separately attributed budget item; actual free-space checks still see its effect. Any follow-up must account for store growth/eviction separately and preserve #14–#17 lifecycle guards. |

Sources for these constraints: [npm ci](https://docs.npmjs.com/cli/v11/commands/npm-ci/),
[pnpm 10 import/store settings](https://pnpm.io/10.x/settings#packageimportmethod),
[pnpm dependency layout](https://pnpm.io/symlinked-node-modules-structure),
[Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html),
[sccache Rust support](https://github.com/mozilla/sccache/blob/main/docs/Rust.md).
Documentation was checked on the measurement date; versioned pnpm 10 references
apply to the pinned fixture tool. Compatibility statements above are documentation
constraints, not claims that all failure cases were reproduced.

## Recommendation

Retain isolation and the project's current npm workflow. #14–#17 reduce accidental
materialization, bound preparation and enable safe output removal without coupling
the lifecycle to a package manager. Keep Cargo's ordinary shared source cache and
private targets; creating per-worktree Cargo homes would waste measured space.

An **opt-in, project-level follow-up** could evaluate pnpm clone-or-copy imports
for projects already using pnpm, with a same-volume store and explicit cache
accounting. Do not make hard-link imports the default based on these numbers.
Validate the real dependency graph, install scripts, lockfile changes and native
addons, plus a filesystem-level physical-space measurement on an isolated volume.

Separately, once `sccache` is available, benchmark a process-local `RUSTC_WRAPPER`,
dedicated `SCCACHE_DIR`, explicit size cap and private targets, comparing cold
misses, cross-checkout hits, source/toolchain changes, concurrency and eviction.
Measure both a no-incremental baseline and the existing incremental workflow;
otherwise a supposed speedup may hide the cost of disabling incremental builds.
Include the cache **and every target directory** in the disk total. Local cache
location and size are configurable. [sccache local storage](https://github.com/mozilla/sccache/blob/main/docs/Local.md)

This investigation does not justify a migration or a claim of faster Rust
rebuilding from a compiler cache. See [combined lifecycle validation](worktree-combined-validation.md)
for the separate desktop and full-check evidence for #13.
