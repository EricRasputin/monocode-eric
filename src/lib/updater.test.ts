import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  announce: vi.fn(),
  ask: vi.fn(),
  check: vi.fn(),
  downloadAndInstall: vi.fn(),
  getName: vi.fn().mockResolvedValue("MonoCode"),
  getVersion: vi.fn(),
  message: vi.fn(),
  relaunch: vi.fn(),
  remember: vi.fn(),
}));

vi.mock("@tauri-apps/api/app", () => ({
  getName: mocks.getName,
  getVersion: mocks.getVersion,
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({
  ask: mocks.ask,
  message: mocks.message,
}));
vi.mock("@tauri-apps/plugin-process", () => ({ relaunch: mocks.relaunch }));
vi.mock("./updateCheck", () => ({ checkForUpdate: mocks.check }));
vi.mock("./sounds", () => ({ announceUpdateAvailable: mocks.announce }));
vi.mock("./updateNotice", () => ({ rememberInstalledUpdate: mocks.remember }));

beforeEach(() => {
  vi.clearAllMocks();
  vi.resetModules();
  mocks.getVersion.mockResolvedValue("0.1.22");
  mocks.getName.mockResolvedValue("MonoCode Fork");
  mocks.relaunch.mockResolvedValue(undefined);
  mocks.message.mockResolvedValue(undefined);
});

describe("Check for Updates", () => {
  it("asks before downloading and leaves the app running when declined", async () => {
    mocks.ask.mockResolvedValue(false);
    const updater = await updaterWithPendingUpdate();

    const result = await updater.runUpdateFlow(true);

    expect(result.phase).toBe("available");
    expect(mocks.ask).toHaveBeenCalledWith(
      expect.stringContaining(
        "MonoCode Fork 0.1.23 is available (you have 0.1.22)",
      ),
      { title: "Update available", kind: "info" },
    );
    expect(mocks.downloadAndInstall).not.toHaveBeenCalled();
    expect(mocks.relaunch).not.toHaveBeenCalled();
  });

  it("downloads after confirmation, reports progress, then restarts", async () => {
    mocks.ask.mockResolvedValue(true);
    mocks.downloadAndInstall.mockImplementation(async (progress) => {
      progress({ event: "Started", data: { contentLength: 100 } });
      progress({ event: "Progress", data: { chunkLength: 50 } });
      progress({ event: "Progress", data: { chunkLength: 50 } });
      progress({ event: "Finished" });
    });
    const updater = await updaterWithPendingUpdate();
    const progress = vi.fn();

    await expect(updater.runUpdateFlow(true, progress)).resolves.toEqual({
      phase: "current",
      currentVersion: "0.1.23",
    });
    expect(mocks.ask.mock.invocationCallOrder[0]).toBeLessThan(
      mocks.downloadAndInstall.mock.invocationCallOrder[0],
    );
    expect(progress).toHaveBeenCalledWith(
      expect.objectContaining({ phase: "downloading", progress: 50 }),
    );
    expect(progress).toHaveBeenCalledWith(
      expect.objectContaining({ phase: "downloading", progress: 100 }),
    );
    expect(mocks.relaunch).toHaveBeenCalledOnce();
  });

  it("reports that the current release is up to date", async () => {
    mocks.check.mockResolvedValue(null);
    const updater = await import("./updater");
    await expect(updater.runUpdateFlow(true)).resolves.toEqual({
      phase: "current",
      currentVersion: "0.1.22",
    });
    expect(mocks.message).toHaveBeenCalledWith(
      "You're on the latest version.",
      { title: "MonoCode Fork" },
    );
    expect(mocks.ask).not.toHaveBeenCalled();
  });

  it("does not restart or report success when signature verification fails", async () => {
    mocks.ask.mockResolvedValue(true);
    mocks.downloadAndInstall.mockRejectedValue(
      new Error("signature verification failed"),
    );
    const updater = await updaterWithPendingUpdate();
    await expect(updater.runUpdateFlow(true)).resolves.toMatchObject({
      phase: "error",
      error: "signature verification failed",
    });
    expect(mocks.relaunch).not.toHaveBeenCalled();
    expect(mocks.remember).not.toHaveBeenCalled();
  });
});

async function updaterWithPendingUpdate() {
  const update = {
    version: "0.1.23",
    downloadAndInstall: mocks.downloadAndInstall,
  };
  mocks.check.mockResolvedValue(update);
  const updater = await import("./updater");
  await updater.probeForUpdate();
  return updater;
}

describe("installPendingUpdate", () => {
  it("records a successful installation before relaunching", async () => {
    mocks.downloadAndInstall.mockResolvedValue(undefined);
    const updater = await updaterWithPendingUpdate();

    await updater.installPendingUpdate();

    expect(mocks.remember).toHaveBeenCalledWith("0.1.23");
    expect(mocks.relaunch).toHaveBeenCalledOnce();
    expect(mocks.remember.mock.invocationCallOrder[0]).toBeLessThan(
      mocks.relaunch.mock.invocationCallOrder[0]!,
    );
  });

  it("does not record or relaunch after installation fails", async () => {
    mocks.downloadAndInstall.mockRejectedValue(new Error("install failed"));
    const updater = await updaterWithPendingUpdate();

    const result = await updater.installPendingUpdate();

    expect(result.phase).toBe("error");
    expect(mocks.remember).not.toHaveBeenCalled();
    expect(mocks.relaunch).not.toHaveBeenCalled();
  });

  it("does not record when no update is pending", async () => {
    const updater = await import("./updater");

    expect((await updater.installPendingUpdate()).phase).toBe("idle");
    expect(mocks.remember).not.toHaveBeenCalled();
    expect(mocks.relaunch).not.toHaveBeenCalled();
  });
});
