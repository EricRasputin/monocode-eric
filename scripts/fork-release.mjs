import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  appendFileSync,
  copyFileSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { basename, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const repository = "EricRasputin/monocode-eric";
const platforms = {
  "aarch64-apple-darwin": "darwin-aarch64",
  "x86_64-apple-darwin": "darwin-x86_64",
};

function versionParts(version) {
  if (
    typeof version !== "string" ||
    version !== version.trim() ||
    !/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.test(version)
  ) {
    throw new Error(`Invalid fork release version: ${version}`);
  }
  const parts = version.split(".").map(Number);
  if (!parts.every(Number.isSafeInteger)) {
    throw new Error(`Invalid fork release version: ${version}`);
  }
  return parts;
}

function compareVersions(left, right) {
  const a = versionParts(left);
  const b = versionParts(right);
  for (let index = 0; index < a.length; index++) {
    if (a[index] !== b[index]) return a[index] - b[index];
  }
  return 0;
}

function releaseTag(version) {
  versionParts(version);
  return `fork-v${version}`;
}

function publishedForkReleases(releases) {
  return releases
    .filter((release) => {
      if (
        release.draft ||
        release.prerelease ||
        !release.tag_name.startsWith("fork-v")
      ) {
        return false;
      }
      try {
        versionParts(release.tag_name.slice(6));
        return true;
      } catch {
        return false;
      }
    })
    .sort((a, b) => compareVersions(b.tag_name.slice(6), a.tag_name.slice(6)));
}

export function nextForkVersion(requested, releases) {
  const latest = publishedForkReleases(releases)[0]?.tag_name.slice(6);
  if (requested) {
    versionParts(requested);
    if (compareVersions(requested, "1.0.0") < 0) {
      throw new Error("New fork releases start at 1.0.0");
    }
    if (latest && compareVersions(requested, latest) <= 0) {
      throw new Error(`Fork version must be newer than ${latest}`);
    }
    return requested;
  }
  if (!latest || compareVersions(latest, "1.0.0") < 0) return "1.0.0";
  const parts = versionParts(latest);
  parts[2]++;
  const next = parts.join(".");
  versionParts(next);
  return next;
}

function digest(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function json(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

export function readUpstreamRelease(root = process.cwd()) {
  const upstream = json(join(root, "upstream-release.json"));
  versionParts(upstream.version);
  if (
    upstream.repository !== "hardbeat920/monocode" ||
    upstream.tag !== `v${upstream.version}` ||
    !/^[a-f0-9]{40}$/.test(upstream.commit) ||
    json(join(root, "package.json")).version !== upstream.version
  ) {
    throw new Error(
      "Upstream release metadata must match the upstream source version and release tag",
    );
  }
  return upstream;
}

function verifyUpstreamBase() {
  const upstream = readUpstreamRelease();
  const taggedCommit = execFileSync(
    "git",
    ["rev-parse", `${upstream.tag}^{commit}`],
    { encoding: "utf8" },
  ).trim();
  if (taggedCommit !== upstream.commit) {
    throw new Error("Recorded upstream commit does not match its release tag");
  }
  try {
    execFileSync("git", [
      "merge-base",
      "--is-ancestor",
      upstream.commit,
      "HEAD",
    ]);
  } catch {
    throw new Error("Build does not contain the recorded upstream release");
  }
  return upstream;
}

function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}

function gitSha() {
  return execFileSync("git", ["rev-parse", "HEAD"], {
    encoding: "utf8",
  }).trim();
}

export function forkChangelogSection(version, messages, upstreamVersion, date) {
  releaseTag(version);
  const entries = messages.map((message) => {
    const paragraphs = message.trim().split(/\n\s*\n/);
    const title =
      /^Merge /.test(paragraphs[0]) && paragraphs[1]
        ? paragraphs[1].split("\n")[0]
        : paragraphs[0].split("\n")[0];
    return `- ${title}`;
  });
  return `## [${version}] - ${date}\n\nBased on upstream MonoCode ${upstreamVersion}.\n\n${entries.join("\n")}\n\n[Full release notes](https://github.com/${repository}/releases/tag/${releaseTag(version)})\n`;
}

function prepare(version) {
  releaseTag(version);
  const upstream = verifyUpstreamBase();
  let previous;
  try {
    previous = execFileSync(
      "git",
      [
        "describe",
        "--tags",
        "--first-parent",
        "--match",
        "fork-v[0-9]*",
        "--abbrev=0",
        "HEAD^",
      ],
      { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
    ).trim();
  } catch {
    // The first release has no preceding fork tag; show recent main changes.
  }
  const args = ["log", "--first-parent", "--format=%B%x00"];
  if (previous) args.push(`${previous}..HEAD`);
  else args.push("-10");
  const messages = execFileSync("git", args, { encoding: "utf8" })
    .split("\0")
    .map((message) => message.trim())
    .filter(Boolean);
  const section = forkChangelogSection(
    version,
    messages,
    upstream.version,
    new Date().toISOString().slice(0, 10),
  );
  const changelog = readFileSync("CHANGELOG.md", "utf8");
  writeFileSync(
    "CHANGELOG.md",
    changelog.replace(/^(# [^\n]+\n)/, (heading) => `${heading}\n${section}\n`),
  );
  if (!readFileSync("CHANGELOG.md", "utf8").includes(`## [${version}]`)) {
    throw new Error(
      "Could not add fork release notes to the bundled changelog",
    );
  }
}

export function stage(version, target, root = process.cwd()) {
  releaseTag(version);
  const platform = platforms[target];
  if (!platform) throw new Error(`Unsupported target: ${target}`);
  const upstream = readUpstreamRelease(root);
  const bundle = join(root, "target", target, "release/bundle");
  const output = join(root, "release-artifacts");
  mkdirSync(output, { recursive: true });
  const archiveSource = join(bundle, "macos/MonoCode Fork.app.tar.gz");
  const signature = readFileSync(`${archiveSource}.sig`, "utf8").trim();
  if (!signature) throw new Error("Missing updater signature");
  const dmgs = readdirSync(join(bundle, "dmg")).filter((name) =>
    name.endsWith(".dmg"),
  );
  if (dmgs.length !== 1) throw new Error("Expected exactly one DMG");
  const stem = `MonoCode-Fork_${version}_${platform}`;
  const archive = `${stem}.app.tar.gz`;
  const dmg = `${stem}.dmg`;
  copyFileSync(archiveSource, join(output, archive));
  copyFileSync(`${archiveSource}.sig`, join(output, `${archive}.sig`));
  copyFileSync(join(bundle, "dmg", dmgs[0]), join(output, dmg));
  writeJson(join(output, `${platform}.json`), {
    version,
    sha: gitSha(),
    upstreamVersion: upstream.version,
    upstream,
    platform,
    archive,
    dmg,
    signature,
    sha256: {
      [archive]: digest(join(output, archive)),
      [dmg]: digest(join(output, dmg)),
    },
  });
}

export function manifest(version, sha, notes, directory = "release-artifacts") {
  const tag = releaseTag(version);
  const upstream = readUpstreamRelease();
  const update = {
    version,
    upstream,
    notes,
    pub_date: new Date().toISOString(),
    platforms: {},
  };
  const assets = [];
  const sums = [];
  for (const platform of Object.values(platforms)) {
    const metadata = json(join(directory, `${platform}.json`));
    if (
      metadata.version !== version ||
      metadata.sha !== sha ||
      metadata.platform !== platform
    ) {
      throw new Error(`Mismatched release metadata for ${platform}`);
    }
    if (
      metadata.upstreamVersion !== upstream.version ||
      Object.entries(upstream).some(
        ([key, value]) => metadata.upstream?.[key] !== value,
      )
    ) {
      throw new Error(`Mismatched upstream release metadata for ${platform}`);
    }
    const stem = `MonoCode-Fork_${version}_${platform}`;
    if (
      metadata.archive !== `${stem}.app.tar.gz` ||
      metadata.dmg !== `${stem}.dmg`
    ) {
      throw new Error(`Unexpected asset names for ${platform}`);
    }
    const signature = readFileSync(
      join(directory, `${metadata.archive}.sig`),
      "utf8",
    ).trim();
    if (!signature || signature !== metadata.signature) {
      throw new Error(`Mismatched updater signature for ${platform}`);
    }
    for (const name of [metadata.archive, metadata.dmg]) {
      const hash = digest(join(directory, name));
      if (
        !statSync(join(directory, name)).size ||
        hash !== metadata.sha256[name]
      ) {
        throw new Error(`Damaged release asset: ${name}`);
      }
      assets.push(join(directory, name));
      sums.push(`${hash}  ${name}`);
    }
    assets.push(join(directory, `${metadata.archive}.sig`));
    update.platforms[platform] = {
      url: `https://github.com/${repository}/releases/download/${tag}/${metadata.archive}`,
      signature,
    };
  }
  writeJson(join(directory, "latest.json"), update);
  writeFileSync(join(directory, "SHA256SUMS"), `${sums.join("\n")}\n`);
  return [
    ...assets,
    join(directory, "latest.json"),
    join(directory, "SHA256SUMS"),
  ];
}

function gh(...args) {
  return execFileSync("gh", args, {
    encoding: "utf8",
    maxBuffer: 16 * 1024 * 1024,
  });
}

export function shouldPublish(version, releases) {
  releaseTag(version);
  return !publishedForkReleases(releases).some(
    (release) => compareVersions(release.tag_name.slice(6), version) >= 0,
  );
}

function assertReleaseSource() {
  if (
    process.env.GITHUB_REPOSITORY !== repository ||
    process.env.GITHUB_REF !== "refs/heads/main"
  ) {
    throw new Error(
      "Fork releases must be published from this repository's main branch",
    );
  }
  const sha = gitSha();
  if (sha !== process.env.GITHUB_SHA)
    throw new Error("Checkout differs from the CI commit");
  return sha;
}

function listReleases() {
  return JSON.parse(
    gh("api", `repos/${repository}/releases`, "--paginate", "--slurp"),
  ).flat();
}

function resolveVersion(requested) {
  const sha = assertReleaseSource();
  const releases = listReleases();
  const previous = publishedForkReleases(releases)[0];
  let alreadyReleased = false;
  if (previous) {
    const previousSha = execFileSync(
      "git",
      ["rev-parse", `${previous.tag_name}^{commit}`],
      { encoding: "utf8" },
    ).trim();
    // A delayed run must not ship an older snapshot with a newer version.
    try {
      execFileSync("git", ["merge-base", "--is-ancestor", previousSha, sha]);
    } catch {
      throw new Error(
        `Release commit must include the latest fork release (${previous.tag_name})`,
      );
    }
    alreadyReleased = previousSha === sha && !requested;
  }
  const version = alreadyReleased
    ? previous.tag_name.slice(6)
    : nextForkVersion(requested, releases);
  const draft = releases.find(
    (release) => release.tag_name === releaseTag(version) && release.draft,
  );
  if (draft && draft.target_commitish !== sha) {
    throw new Error(
      `Draft fork-v${version} belongs to another commit; choose a different fork version`,
    );
  }
  if (!process.env.GITHUB_OUTPUT) throw new Error("GITHUB_OUTPUT is required");
  appendFileSync(
    process.env.GITHUB_OUTPUT,
    `version=${version}\npublish=${!alreadyReleased}\n`,
  );
  console.log(
    alreadyReleased
      ? `This commit is already released as ${version}; nothing to publish.`
      : `Selected MonoCode Fork ${version} for ${sha}`,
  );
}

function publish(version) {
  const tag = releaseTag(version);
  const sha = assertReleaseSource();
  const releases = listReleases();
  if (!shouldPublish(version, releases)) {
    console.log(
      "This version or a newer fork update is already published; leaving it unchanged.",
    );
    return;
  }
  const previous = publishedForkReleases(releases)[0];
  const notesArgs = [
    "api",
    `repos/${repository}/releases/generate-notes`,
    "-f",
    `tag_name=${tag}`,
    "-f",
    `target_commitish=${sha}`,
  ];
  if (previous) notesArgs.push("-f", `previous_tag_name=${previous.tag_name}`);
  const generated = JSON.parse(gh(...notesArgs)).body;
  const upstream = readUpstreamRelease();
  const title = `MonoCode Fork ${version} (${upstream.version})`;
  const notes = `${title}.\n\nBased on [upstream MonoCode ${upstream.version}](https://github.com/${upstream.repository}/releases/tag/${upstream.tag}), commit \`${upstream.commit}\`.\n\n${generated}`;
  const assets = manifest(version, sha, notes);
  const notesFile = "release-artifacts/release-notes.md";
  writeFileSync(notesFile, `${notes}\n`);
  const draft = releases.find((release) => release.tag_name === tag);
  if (draft) {
    if (!draft.draft || draft.target_commitish !== sha) {
      throw new Error("Existing release does not match this draft and commit");
    }
    gh("release", "edit", tag, "--repo", repository, "--notes-file", notesFile);
    gh("release", "upload", tag, ...assets, "--repo", repository, "--clobber");
  } else {
    gh(
      "release",
      "create",
      tag,
      ...assets,
      "--repo",
      repository,
      "--target",
      sha,
      "--draft",
      "--title",
      title,
      "--notes-file",
      notesFile,
    );
  }
  const uploaded = JSON.parse(
    gh("release", "view", tag, "--repo", repository, "--json", "assets"),
  ).assets;
  for (const path of assets) {
    if (
      !uploaded.some(
        (asset) =>
          asset.name === basename(path) && asset.size === statSync(path).size,
      )
    ) {
      throw new Error(`Release upload incomplete: ${basename(path)}`);
    }
  }
  gh("release", "edit", tag, "--repo", repository, "--draft=false", "--latest");
  console.log(`Published https://github.com/${repository}/releases/tag/${tag}`);
}

if (
  process.argv[1] &&
  resolve(process.argv[1]) === fileURLToPath(import.meta.url)
) {
  const [command, version, target] = process.argv.slice(2);
  if (command === "version") resolveVersion(version);
  else if (command === "prepare") prepare(version);
  else if (command === "stage") stage(version, target);
  else if (command === "publish") publish(version);
  else
    throw new Error(
      "Usage: node scripts/fork-release.mjs <version|prepare|stage|publish> [version] [target]",
    );
}
