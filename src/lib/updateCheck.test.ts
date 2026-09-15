// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from "vitest";
import { clearMocks, mockIPC } from "@tauri-apps/api/mocks";
import { Update } from "@tauri-apps/plugin-updater";
import { checkForUpdate } from "./updateCheck";

afterEach(clearMocks);

describe("native update checks", () => {
  it("uses the bounded native check and preserves Tauri's installer resource", async () => {
    const ipc = vi.fn((command: string) => {
      if (command === "check_for_update") {
        return {
          rid: 42,
          currentVersion: "0.2.12",
          version: "1.0.0",
          date: "2026-09-15T00:00:00Z",
          body: "All fork changes",
          rawJson: { version: "1.0.0" },
        };
      }
    });
    mockIPC(ipc);

    const update = await checkForUpdate();
    expect(update).toBeInstanceOf(Update);
    expect(update).toMatchObject({ rid: 42, version: "1.0.0", body: "All fork changes" });
    await update!.downloadAndInstall();
    expect(ipc).toHaveBeenCalledWith(
      "plugin:updater|download_and_install",
      expect.objectContaining({ rid: 42 }),
    );
  });

  it("keeps network failures distinct from an up-to-date response and allows retry", async () => {
    const ipc = vi.fn().mockRejectedValueOnce("request timed out").mockResolvedValueOnce(null);
    mockIPC(ipc);
    await expect(checkForUpdate()).rejects.toBe("request timed out");
    await expect(checkForUpdate()).resolves.toBeNull();
    expect(ipc).toHaveBeenCalledTimes(2);
  });
});
