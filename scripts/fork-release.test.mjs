import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";
import { manifest, shouldPublish } from "./fork-release.mjs";

function fixture(t) {
  const dir = mkdtempSync(join(tmpdir(), "fork-release-test-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const metadata = {};
  for (const platform of ["darwin-aarch64", "darwin-x86_64"]) {
    const stem = `MonoCode-Fork_0.2.7_${platform}`;
    const archive = `${stem}.app.tar.gz`;
    const dmg = `${stem}.dmg`;
    const contents = `build 7 for ${platform}`;
    const hash = createHash("sha256").update(contents).digest("hex");
    const signature = `signature-for-${platform}`;
    writeFileSync(join(dir, archive), contents);
    writeFileSync(join(dir, dmg), contents);
    writeFileSync(join(dir, `${archive}.sig`), signature);
    metadata[platform] = {
      version: "0.2.7",
      sha: "commit-7",
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
    "0.2.7",
    "commit-7",
    "New worktree improvements",
    dir,
  );
  const feed = JSON.parse(readFileSync(join(dir, "latest.json")));
  assert.equal(assets.length, 8);
  assert.equal(feed.version, "0.2.7");
  assert.equal(feed.notes, "New worktree improvements");
  assert.deepEqual(Object.keys(feed.platforms), [
    "darwin-aarch64",
    "darwin-x86_64",
  ]);
  for (const [platform, item] of Object.entries(feed.platforms)) {
    assert.equal(item.signature, `signature-for-${platform}`);
    assert.equal(
      item.url,
      `https://github.com/EricRasputin/monocode-eric/releases/download/fork-v0.2.7/MonoCode-Fork_0.2.7_${platform}.app.tar.gz`,
    );
  }
});

test("does not produce an update feed if either architecture is missing", (t) => {
  const { dir } = fixture(t);
  rmSync(join(dir, "darwin-x86_64.json"));
  assert.throws(() => manifest("0.2.7", "commit-7", "", dir), /ENOENT/);
  assert.throws(() => readFileSync(join(dir, "latest.json")), /ENOENT/);
});

test("rejects mixed commits and versions", (t) => {
  const { dir } = fixture(t);
  assert.throws(
    () => manifest("0.2.7", "commit-8", "", dir),
    /Mismatched release metadata/,
  );
  assert.throws(
    () => manifest("0.2.8", "commit-7", "", dir),
    /Mismatched release metadata/,
  );
});

test("rejects a changed bundle or signature after staging", (t) => {
  const { dir, metadata } = fixture(t);
  const arm = metadata["darwin-aarch64"];
  writeFileSync(join(dir, arm.archive), "corrupted download");
  assert.throws(
    () => manifest("0.2.7", "commit-7", "", dir),
    /Damaged release asset/,
  );
  writeFileSync(join(dir, `${arm.archive}.sig`), "different signature");
  assert.throws(
    () => manifest("0.2.7", "commit-7", "", dir),
    /Mismatched updater signature/,
  );
});

test("reruns and late older builds cannot replace a published newer update", () => {
  const release = (build, draft = false) => ({
    tag_name: `fork-v0.2.${build}`,
    draft,
    prerelease: false,
  });
  assert.equal(shouldPublish("0.2.7", []), true);
  assert.equal(shouldPublish("0.2.7", [release(7, true)]), true);
  assert.equal(shouldPublish("0.2.7", [release(6)]), true);
  assert.equal(shouldPublish("0.2.7", [release(7)]), false);
  assert.equal(shouldPublish("0.2.7", [release(10)]), false);
  assert.equal(shouldPublish("0.2.10", [release(9)]), true);
  assert.throws(
    () => shouldPublish("0.1.46", []),
    /Invalid fork release version/,
  );
});
