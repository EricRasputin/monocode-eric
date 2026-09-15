import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  forkChangelogSection,
  forkCommitMessages,
  manifest,
  nextForkVersion,
  readUpstreamRelease,
  shouldPublish,
} from "./fork-release.mjs";

const upstream = JSON.parse(
  readFileSync(new URL("../upstream-release.json", import.meta.url)),
);

test("bundled notes use the fork version and human-readable merge titles", () => {
  const section = forkChangelogSection(
    "1.0.7",
    [
      "Merge pull request #8 from owner/updates\n\nEnable in-app updates",
      "Fix session recovery\n\nPreserve archived conversations.",
    ],
    "0.1.46",
    "2026-09-14",
  );
  assert.match(section, /^## \[1\.0\.7\] - 2026-09-14\n/m);
  assert.match(section, /- Enable in-app updates\n- Fix session recovery/);
  assert.match(section, /upstream MonoCode 0\.1\.46/);
  assert.ok(!section.includes("Merge pull request"));
  assert.match(section, /releases\/tag\/fork-v1\.0\.7/);
});

test("release notes include local commits behind merges and exclude prior releases and upstream", (t) => {
  const dir = mkdtempSync(join(tmpdir(), "fork-history-test-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const git = (...args) => execFileSync("git", args, { cwd: dir, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
  git("init", "-b", "main");
  git("config", "user.name", "Release test");
  git("config", "user.email", "release-test@example.com");
  git("commit", "--allow-empty", "-m", "Upstream base");
  git("branch", "upstream");
  git("commit", "--allow-empty", "-m", "Previously released fork change");
  git("tag", "fork-v0.2.12");
  git("switch", "-c", "local-work");
  git("commit", "--allow-empty", "-m", "Add checkout capacity");
  git("commit", "--allow-empty", "-m", "Add reviewed output cleanup");
  git("switch", "upstream");
  git("commit", "--allow-empty", "-m", "Upstream release change");
  const upstreamCommit = git("rev-parse", "HEAD");
  git("switch", "local-work");
  git("merge", "upstream", "--no-ff", "-m", "Merge new upstream release");
  git("switch", "main");
  git("merge", "local-work", "--no-ff", "-m", "Merge all fork changes");
  git("commit", "--allow-empty", "-m", "Bound update check connections");

  const messages = forkCommitMessages("fork-v0.2.12", upstreamCommit, dir);
  assert.deepEqual(messages.sort(), [
    "Add checkout capacity",
    "Add reviewed output cleanup",
    "Bound update check connections",
  ]);
});

function fixture(t) {
  const dir = mkdtempSync(join(tmpdir(), "fork-release-test-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const metadata = {};
  for (const platform of ["darwin-aarch64", "darwin-x86_64"]) {
    const stem = `MonoCode-Fork_1.0.7_${platform}`;
    const archive = `${stem}.app.tar.gz`;
    const dmg = `${stem}.dmg`;
    const contents = `build 7 for ${platform}`;
    const hash = createHash("sha256").update(contents).digest("hex");
    const signature = `signature-for-${platform}`;
    writeFileSync(join(dir, archive), contents);
    writeFileSync(join(dir, dmg), contents);
    writeFileSync(join(dir, `${archive}.sig`), signature);
    metadata[platform] = {
      version: "1.0.7",
      sha: "commit-7",
      upstreamVersion: upstream.version,
      upstream,
      platform,
      archive,
      dmg,
      signature,
      sha256: { [archive]: hash, [dmg]: hash },
    };
    writeFileSync(
      join(dir, `${platform}.json`),
      JSON.stringify(metadata[platform]),
    );
  }
  return { dir, metadata };
}

test("publishes a complete feed with immutable URLs and architecture-specific signatures", (t) => {
  const { dir } = fixture(t);
  const assets = manifest(
    "1.0.7",
    "commit-7",
    "New worktree improvements",
    dir,
  );
  const feed = JSON.parse(readFileSync(join(dir, "latest.json")));
  assert.equal(assets.length, 8);
  assert.equal(feed.version, "1.0.7");
  assert.deepEqual(feed.upstream, upstream);
  assert.equal(feed.notes, "New worktree improvements");
  assert.deepEqual(Object.keys(feed.platforms), [
    "darwin-aarch64",
    "darwin-x86_64",
  ]);
  for (const [platform, item] of Object.entries(feed.platforms)) {
    assert.equal(item.signature, `signature-for-${platform}`);
    assert.equal(
      item.url,
      `https://github.com/EricRasputin/monocode-eric/releases/download/fork-v1.0.7/MonoCode-Fork_1.0.7_${platform}.app.tar.gz`,
    );
  }
});

test("does not produce an update feed if either architecture is missing", (t) => {
  const { dir } = fixture(t);
  rmSync(join(dir, "darwin-x86_64.json"));
  assert.throws(() => manifest("1.0.7", "commit-7", "", dir), /ENOENT/);
  assert.throws(() => readFileSync(join(dir, "latest.json")), /ENOENT/);
});

test("rejects mixed commits and versions", (t) => {
  const { dir } = fixture(t);
  assert.throws(
    () => manifest("1.0.7", "commit-8", "", dir),
    /Mismatched release metadata/,
  );
  assert.throws(
    () => manifest("1.0.8", "commit-7", "", dir),
    /Mismatched release metadata/,
  );
});

test("rejects packages that disagree about the upstream release base", (t) => {
  const { dir, metadata } = fixture(t);
  metadata["darwin-x86_64"].upstream = { ...upstream, commit: "0".repeat(40) };
  writeFileSync(
    join(dir, "darwin-x86_64.json"),
    JSON.stringify(metadata["darwin-x86_64"]),
  );
  assert.throws(
    () => manifest("1.0.7", "commit-7", "", dir),
    /Mismatched upstream release metadata/,
  );
});

test("upstream provenance must match the source version and an exact release tag", (t) => {
  const dir = mkdtempSync(join(tmpdir(), "upstream-metadata-test-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify({ version: upstream.version }),
  );
  writeFileSync(join(dir, "upstream-release.json"), JSON.stringify(upstream));
  assert.deepEqual(readUpstreamRelease(dir), upstream);
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify({ version: "99.0.0" }),
  );
  assert.throws(
    () => readUpstreamRelease(dir),
    /must match the upstream source/,
  );
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify({ version: upstream.version }),
  );
  writeFileSync(
    join(dir, "upstream-release.json"),
    JSON.stringify({ ...upstream, tag: "main" }),
  );
  assert.throws(
    () => readUpstreamRelease(dir),
    /must match the upstream source/,
  );
});

test("rejects a changed bundle or signature after staging", (t) => {
  const { dir, metadata } = fixture(t);
  const arm = metadata["darwin-aarch64"];
  writeFileSync(join(dir, arm.archive), "corrupted download");
  assert.throws(
    () => manifest("1.0.7", "commit-7", "", dir),
    /Damaged release asset/,
  );
  writeFileSync(join(dir, `${arm.archive}.sig`), "different signature");
  assert.throws(
    () => manifest("1.0.7", "commit-7", "", dir),
    /Mismatched updater signature/,
  );
});

test("reruns and late older builds cannot replace a published newer update", () => {
  const release = (build, draft = false) => ({
    tag_name: `fork-v1.0.${build}`,
    draft,
    prerelease: false,
  });
  assert.equal(shouldPublish("1.0.7", []), true);
  assert.equal(shouldPublish("1.0.7", [release(7, true)]), true);
  assert.equal(shouldPublish("1.0.7", [release(6)]), true);
  assert.equal(shouldPublish("1.0.7", [release(7)]), false);
  assert.equal(shouldPublish("1.0.7", [release(10)]), false);
  assert.equal(shouldPublish("1.0.10", [release(9)]), true);
  assert.throws(
    () => shouldPublish("v1.0.0", []),
    /Invalid fork release version/,
  );
});

const release = (version, overrides = {}) => ({
  tag_name: `fork-v${version}`,
  draft: false,
  prerelease: false,
  ...overrides,
});

test("fork numbering starts at 1.0.0 and ignores upstream, drafts, and prereleases", () => {
  assert.equal(nextForkVersion("", []), "1.0.0");
  assert.equal(nextForkVersion("", [release("0.2.12")]), "1.0.0");
  assert.equal(
    nextForkVersion("", [
      release("1.0.0"),
      release("99.0.0", { tag_name: "v99.0.0" }),
      release("2.0.0", { draft: true }),
      release("3.0.0", { prerelease: true }),
      release("4.0.0-beta.1"),
      release("not-a-version"),
    ]),
    "1.0.1",
  );
});

test("fork versions advance independently and support explicit minor and major releases", () => {
  const published = [release("1.9.12"), release("1.10.2"), release("0.2.99")];
  assert.equal(nextForkVersion("", published), "1.10.3");
  assert.equal(nextForkVersion("1.11.0", published), "1.11.0");
  assert.equal(nextForkVersion("2.0.0", published), "2.0.0");
  assert.equal(shouldPublish("1.0.0", [release("0.2.99")]), true);
  assert.equal(shouldPublish("1.9.99", published), false);
  assert.equal(shouldPublish("2.0.0", published), true);
});

test("rejects duplicate, older, malformed, and unsafe version numbers", () => {
  for (const version of ["1.0.0", "0.2.13"]) {
    assert.throws(() => nextForkVersion(version, [release("1.0.0")]));
  }
  for (const version of [
    "v1.0.0",
    "1.01.0",
    "1.0",
    "1.0.0-beta.1",
    "1.0.0+fork",
    "1.0.0\n",
    "1.0.9007199254740992",
  ]) {
    assert.throws(
      () => nextForkVersion(version, []),
      /Invalid fork release version/,
    );
  }
});

function gitFixture(t) {
  const dir = mkdtempSync(join(tmpdir(), "fork-version-test-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const git = (...args) =>
    execFileSync("git", args, {
      cwd: dir,
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    }).trim();
  git("init");
  git("config", "user.name", "Release Test");
  git("config", "user.email", "release-test@example.invalid");
  git("config", "commit.gpgsign", "false");
  git("config", "tag.gpgsign", "false");
  const commit = (message) => {
    git("commit", "--allow-empty", "-m", message);
    return git("rev-parse", "HEAD");
  };
  const first = commit("First fork release");
  git("tag", "fork-v1.0.0");
  const releasesFile = join(dir, "releases.json");
  writeFileSync(
    releasesFile,
    JSON.stringify([[release("1.0.0", { target_commitish: first })]]),
  );
  // Replace network reads; exercise ancestry and tag checks against a real git repository.
  const preload = join(dir, "offline-gh.mjs");
  writeFileSync(
    preload,
    `
    import childProcess from "node:child_process";
    import { readFileSync } from "node:fs";
    import { syncBuiltinESMExports } from "node:module";
    const original = childProcess.execFileSync;
    childProcess.execFileSync = (command, ...args) => {
      if (command === "gh") return readFileSync(process.env.FORK_TEST_RELEASES, "utf8");
      if (command === "git" && args[0][0] === "ls-remote") {
        const ref = args[0][2];
        const sha = original("git", ["rev-parse", ref + "^{commit}"], { encoding: "utf8" }).trim();
        return sha + "\\t" + ref + "^{}\\n";
      }
      return original(command, ...args);
    };
    syncBuiltinESMExports();
  `,
  );
  const output = join(dir, "output.txt");
  const script = fileURLToPath(new URL("./fork-release.mjs", import.meta.url));
  const run = (command, version = "") =>
    execFileSync(
      process.execPath,
      ["--import", pathToFileURL(preload).href, script, command, version],
      {
        cwd: dir,
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
        env: {
          ...process.env,
          GITHUB_REPOSITORY: "EricRasputin/monocode-eric",
          GITHUB_REF: "refs/heads/main",
          GITHUB_SHA: git("rev-parse", "HEAD"),
          GITHUB_OUTPUT: output,
          FORK_TEST_RELEASES: releasesFile,
        },
      },
    );
  return { dir, git, commit, first, releasesFile, output, run };
}

test("version preflight skips an already-released commit and selects the next batch", (t) => {
  const fixture = gitFixture(t);
  fixture.run("version");
  assert.match(
    readFileSync(fixture.output, "utf8"),
    /version=1.0.0\npublish=false/,
  );
  fixture.commit("Batch of improvements");
  fixture.run("version");
  assert.match(
    readFileSync(fixture.output, "utf8"),
    /version=1.0.1\npublish=true/,
  );
  fixture.run("version", "1.1.0");
  assert.match(
    readFileSync(fixture.output, "utf8"),
    /version=1.1.0\npublish=true/,
  );
});

test("a failed draft can be retried only for its original commit", (t) => {
  const fixture = gitFixture(t);
  const candidate = fixture.commit("Release candidate");
  writeFileSync(
    fixture.releasesFile,
    JSON.stringify([
      [
        release("1.0.0", { target_commitish: fixture.first }),
        release("1.0.1", { draft: true, target_commitish: candidate }),
      ],
    ]),
  );
  fixture.run("version");
  const selected = readFileSync(fixture.output, "utf8");
  assert.match(selected, /version=1.0.1\npublish=true/);
  fixture.commit("Different release candidate");
  assert.throws(() => fixture.run("version"), /belongs to another commit/);
  assert.equal(readFileSync(fixture.output, "utf8"), selected);
});

test("version preflight rejects an old commit after a newer release has shipped", (t) => {
  const fixture = gitFixture(t);
  const newer = fixture.commit("Newer release");
  fixture.git("tag", "fork-v1.0.1");
  writeFileSync(
    fixture.releasesFile,
    JSON.stringify([[release("1.0.1", { target_commitish: newer })]]),
  );
  fixture.git("checkout", "--detach", fixture.first);
  assert.throws(() => fixture.run("version"));
  assert.equal(existsSync(fixture.output), false);
});

test("bundled notes include the batch since the last independently numbered fork release", (t) => {
  const fixture = gitFixture(t);
  fixture.commit("First fix in the batch");
  fixture.commit("Second fix in the batch");
  writeFileSync(
    join(fixture.dir, "package.json"),
    JSON.stringify({ version: "9.8.7" }),
  );
  fixture.git("tag", "v9.8.7", fixture.first);
  writeFileSync(
    join(fixture.dir, "upstream-release.json"),
    JSON.stringify({
      repository: "hardbeat920/monocode",
      tag: "v9.8.7",
      version: "9.8.7",
      commit: fixture.first,
    }),
  );
  writeFileSync(
    join(fixture.dir, "CHANGELOG.md"),
    "# Changelog\n\nUpstream history\n",
  );
  fixture.run("prepare", "1.0.1");
  const notes = readFileSync(join(fixture.dir, "CHANGELOG.md"), "utf8");
  assert.match(notes, /## \[1\.0\.1\]/);
  assert.match(notes, /upstream MonoCode 9\.8\.7/);
  assert.match(notes, /First fix in the batch/);
  assert.match(notes, /Second fix in the batch/);
  assert.ok(!notes.includes("First fork release"));
  writeFileSync(
    join(fixture.dir, "upstream-release.json"),
    JSON.stringify({
      repository: "hardbeat920/monocode",
      tag: "v9.8.7",
      version: "9.8.7",
      commit: "0".repeat(40),
    }),
  );
  assert.throws(
    () => fixture.run("prepare", "1.0.1"),
    /does not match its release tag/,
  );
});
