import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
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

function releaseTag(version) {
  if (!/^0\.2\.[1-9]\d*$/.test(version)) {
    throw new Error(`Invalid fork release version: ${version}`);
  }
  return `fork-v${version}`;
}

function digest(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function json(path) {
  return JSON.parse(readFileSync(path, "utf8"));
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
  let previous;
  try {
    previous = execFileSync(
      "git",
      [
        "describe",
        "--tags",
        "--first-parent",
        "--match",
        "fork-v0.2.*",
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
    json("package.json").version,
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
    upstreamVersion: json(join(root, "package.json")).version,
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
  const update = {
    version,
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
  const build = Number(version.split(".")[2]);
  return !releases.some((release) => {
    const match = /^fork-v0\.2\.([1-9]\d*)$/.exec(release.tag_name);
    return (
      !release.draft &&
      !release.prerelease &&
      match &&
      Number(match[1]) >= build
    );
  });
}

function publish(version) {
  const tag = releaseTag(version);
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
  const releases = JSON.parse(
    gh("api", `repos/${repository}/releases`, "--paginate", "--slurp"),
  ).flat();
  if (!shouldPublish(version, releases)) {
    console.log(
      "This version or a newer fork update is already published; leaving it unchanged.",
    );
    return;
  }
  const previous = releases.find(
    (release) => !release.draft && /^fork-v0\.2\./.test(release.tag_name),
  );
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
  const upstream = json("package.json").version;
  const notes = `MonoCode Fork ${version} (upstream ${upstream}).\n\n${generated}`;
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
      `MonoCode Fork ${version}`,
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
  if (command === "prepare") prepare(version);
  else if (command === "stage") stage(version, target);
  else if (command === "publish") publish(version);
  else
    throw new Error(
      "Usage: node scripts/fork-release.mjs <prepare|stage|publish> <version> [target]",
    );
}
