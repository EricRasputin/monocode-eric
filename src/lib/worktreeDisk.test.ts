// @vitest-environment happy-dom
import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import {
  DISK_GIB,
  diskBytes,
  diskWarnings,
  invokeWorkspace,
  WorkspaceCapacityError,
  type CapacityFailure,
  type DiskSnapshot,
} from "./worktreeDisk";
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
const snapshot = (): DiskSnapshot => ({
  schemaVersion: 1,
  settings: {
    schemaVersion: 1,
    version: 0,
    checkoutBudgetBytes: 30 * DISK_GIB,
    minimumFreeBytes: 10 * DISK_GIB,
    initialAllowanceBytes: 5 * DISK_GIB,
  },
  measuredAt: 1000,
  complete: true,
  usedBytes: 20 * DISK_GIB,
  reclaimableBytes: 2 * DISK_GIB,
  pendingBytes: 4 * DISK_GIB,
  checkouts: [],
  volumes: [
    {
      id: "device:a",
      path: "/managed",
      availableBytes: 20 * DISK_GIB,
      measuredAt: 1000,
    },
  ],
  reservations: [
    {
      token: "one",
      path: "/managed/one",
      operation: "setup",
      targetBytes: 5 * DISK_GIB,
      remainingBytes: 4 * DISK_GIB,
      volumeIds: ["device:a"],
      createdAt: 900,
    },
  ],
  limitations: [],
});
beforeEach(() => vi.clearAllMocks());
describe("disk capacity explanations", () => {
  it("warns at 80 percent and at the exact budget, counting pending only once", () => {
    const s = snapshot();
    expect(diskWarnings(s)).toEqual([
      expect.stringContaining("near the budget"),
    ]);
    s.usedBytes = 26 * DISK_GIB;
    expect(diskWarnings(s)).toEqual([
      expect.stringContaining("reached the budget"),
    ]);
    s.usedBytes = 19 * DISK_GIB;
    expect(diskWarnings(s)).toEqual([]);
  });
  it("compares free space and pending reservations on the correct volume", () => {
    const s = snapshot();
    s.settings.checkoutBudgetBytes = null;
    s.volumes[0].availableBytes = 14 * DISK_GIB;
    expect(diskWarnings(s)[0]).toContain("Low free space");
    s.reservations[0].volumeIds = ["device:other"];
    expect(diskWarnings(s)).toEqual([]);
    s.settings.minimumFreeBytes = null;
    s.volumes[0].availableBytes = 0;
    expect(diskWarnings(s)[0]).toContain("Low free space");
  });
  it("uses newer free-space probes even while the checkout scan is stale", () => {
    const s = snapshot();
    s.settings.checkoutBudgetBytes = null;
    const newer = {
      settings: s.settings,
      measuredAt: 31_000,
      volumes: [
        { ...s.volumes[0], availableBytes: 3 * DISK_GIB, measuredAt: 31_000 },
      ],
    };
    expect(diskWarnings(s, newer)[0]).toContain("3 GiB available");
    expect(diskWarnings(s, { ...newer, measuredAt: 1 })).toEqual([]);
    s.complete = false;
    expect(diskWarnings(s)[0]).toContain("incomplete");
  });
  it("explains structured failures and never retries with a changed workspace", async () => {
    const failure: CapacityFailure = {
      code: "WORKTREE_CAPACITY",
      reason: "checkoutBudget",
      operation: "restore",
      path: "/managed/saved",
      requiredBytes: 5 * DISK_GIB,
      volumeId: null,
      snapshot: snapshot(),
      guidance: [],
    };
    vi.mocked(invoke).mockRejectedValue(failure);
    const received = vi.fn();
    window.addEventListener("worktree-capacity-failure", received);
    const args = {
      request: { path: failure.path, createNew: false, useWorktree: true },
    };
    await expect(
      invokeWorkspace("worktree_prepare", args),
    ).rejects.toBeInstanceOf(WorkspaceCapacityError);
    expect(invoke).toHaveBeenCalledExactlyOnceWith("worktree_prepare", args);
    expect(received).toHaveBeenCalledOnce();
    const error = new WorkspaceCapacityError(failure);
    expect(error.message).toContain("Settings → Worktrees");
    expect(error.message).toContain("explicitly reuse");
    window.removeEventListener("worktree-capacity-failure", received);
  });
  it("allows native ready access and preserves unrelated errors", async () => {
    vi.mocked(invoke).mockResolvedValue(undefined);
    await expect(
      invokeWorkspace("worktree_setup", { path: "/ready" }),
    ).resolves.toBeUndefined();
    vi.mocked(invoke).mockRejectedValue("setup failed");
    await expect(
      invokeWorkspace("worktree_setup", { path: "/ready" }),
    ).rejects.toBe("setup failed");
    expect(diskBytes(0)).toBe("0 GiB");
    expect(diskBytes(1)).toBe("<0.01 GiB");
  });
});

it("clears a previous failure only after setup succeeds, not when preparation locates a folder", async () => {
  const ready = vi.fn();
  window.addEventListener("worktree-capacity-ready", ready);
  vi.mocked(invoke).mockResolvedValue("/managed/pending");
  await invokeWorkspace("worktree_prepare", {
    request: { path: "/managed/pending" },
  });
  expect(ready).not.toHaveBeenCalled();
  vi.mocked(invoke).mockResolvedValue(undefined);
  await invokeWorkspace("worktree_setup", { path: "/managed/pending" });
  expect(ready).toHaveBeenCalledOnce();
  window.removeEventListener("worktree-capacity-ready", ready);
});
