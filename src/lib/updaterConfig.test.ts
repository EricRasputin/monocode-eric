import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { getName, getVersion, check, message, ask, relaunch } = vi.hoisted(
  () => ({
    getName: vi.fn().mockResolvedValue("MonoCode"),
    getVersion: vi.fn(),
    check: vi.fn(),
    message: vi.fn(),
    ask: vi.fn(),
    relaunch: vi.fn(),
  }),
);

vi.mock("@tauri-apps/api/app", () => ({ getName, getVersion }));
vi.mock("./updateCheck", () => ({ checkForUpdate: check }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ ask, message }));
vi.mock("@tauri-apps/plugin-process", () => ({ relaunch }));
vi.mock("./sounds", () => ({ announceUpdateAvailable: vi.fn() }));

import { runUpdateFlow } from "./updater";

describe("updater", () => {
  beforeEach(() => getName.mockResolvedValue("MonoCode"));
  afterEach(() => {
    vi.resetAllMocks();
  });

  it("keeps automatic checks quiet when updater endpoints are missing", async () => {
    getVersion.mockResolvedValue("0.1.23");
    check.mockRejectedValue(
      new Error("Updater does not have any endpoints set"),
    );

    await expect(runUpdateFlow(false)).resolves.toEqual({
      phase: "idle",
      currentVersion: "0.1.23",
    });
    expect(message).not.toHaveBeenCalled();
  });

  it("points manual checks without updater endpoints to GitHub releases", async () => {
    getVersion.mockResolvedValue("0.1.23");
    check.mockRejectedValue(
      new Error("Updater does not have any endpoints set"),
    );

    await expect(runUpdateFlow(true)).resolves.toEqual({
      phase: "idle",
      currentVersion: "0.1.23",
    });
    expect(message).toHaveBeenCalledWith(
      expect.stringContaining(
        "https://github.com/hardbeat920/monocode/releases/latest",
      ),
      { title: "MonoCode" },
    );
  });

  it("still reports real updater failures", async () => {
    getVersion.mockResolvedValue("0.1.23");
    check.mockRejectedValue(new Error("network failed"));

    await expect(runUpdateFlow(true)).resolves.toMatchObject({
      phase: "error",
      error: "network failed",
    });
    expect(message).toHaveBeenCalledOnce();
  });

  it("points fork builds to fork releases when updates are not configured", async () => {
    getName.mockResolvedValue("MonoCode Fork");
    getVersion.mockResolvedValue("0.2.0");
    check.mockRejectedValue(
      new Error("Updater does not have any endpoints set"),
    );
    await runUpdateFlow(true);
    expect(message).toHaveBeenCalledWith(
      expect.stringContaining(
        "https://github.com/EricRasputin/monocode-eric/releases/latest",
      ),
      { title: "MonoCode Fork" },
    );
  });
});
