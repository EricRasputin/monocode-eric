import { beforeEach, describe, expect, it, vi } from "vitest";
import upstreamRelease from "../../upstream-release.json";

const mocks = vi.hoisted(() => ({
  getIdentifier: vi.fn(),
  readAppVersion: vi.fn(),
}));
vi.mock("@tauri-apps/api/app", () => ({ getIdentifier: mocks.getIdentifier }));
vi.mock("./updater", () => ({ readAppVersion: mocks.readAppVersion }));

import { formatAppVersion, readAppBuildInfo } from "./appVersion";

beforeEach(() => {
  vi.resetAllMocks();
  mocks.readAppVersion.mockResolvedValue("1.0.0");
});

describe("installed app version provenance", () => {
  it("pairs the fork version with its bundled upstream base while retaining the raw update version", async () => {
    mocks.getIdentifier.mockResolvedValue("com.monocode.fork.worktrees");
    const info = await readAppBuildInfo();
    expect(info.version).toBe("1.0.0");
    expect(info.upstream).toEqual(upstreamRelease);
    expect(formatAppVersion(info.version, info.upstream?.version)).toBe(
      `1.0.0 (${upstreamRelease.version})`,
    );
  });

  it("does not label upstream app builds with the fork's recorded base", async () => {
    mocks.getIdentifier.mockResolvedValue("com.monocode.desktop");
    const info = await readAppBuildInfo();
    expect(info.upstream).toBeUndefined();
    expect(formatAppVersion(info.version, info.upstream?.version)).toBe(
      "1.0.0",
    );
  });

  it("keeps the app version available when the native identity cannot be read", async () => {
    mocks.getIdentifier.mockRejectedValue(new Error("not running in Tauri"));
    await expect(readAppBuildInfo()).resolves.toEqual({
      version: "1.0.0",
      upstream: undefined,
    });
  });
});
