import { invoke } from "@tauri-apps/api/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  getRecoveryStorage,
  RECOVERY_STORAGE_MIB,
  recoveryStoragePressure,
  setRecoveryStorageLimit,
} from "./worktreeStorage";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

beforeEach(() => vi.clearAllMocks());

describe("worktree recovery storage", () => {
  it("uses the standalone get and compare-and-set command contracts", async () => {
    vi.mocked(invoke).mockResolvedValue({});

    await getRecoveryStorage();
    await setRecoveryStorageLimit(96 * RECOVERY_STORAGE_MIB, 7);

    expect(invoke).toHaveBeenNthCalledWith(1, "worktree_storage_get");
    expect(invoke).toHaveBeenNthCalledWith(2, "worktree_storage_limit_set", {
      limitBytes: 96 * RECOVERY_STORAGE_MIB,
      expectedVersion: 7,
    });
  });

  it("changes pressure at 80 and 100 percent", () => {
    expect(recoveryStoragePressure(79, 100)).toBe("normal");
    expect(recoveryStoragePressure(80, 100)).toBe("warning");
    expect(recoveryStoragePressure(100, 100)).toBe("full");
  });
});
