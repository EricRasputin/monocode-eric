// @vitest-environment happy-dom
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { listen } from "@tauri-apps/api/event";
import {
  DISK_GIB,
  getWorktreeDisk,
  saveWorktreeDiskSettings,
  type DiskSnapshot,
} from "../lib/worktreeDisk";
import { WorktreeDiskSettings } from "./WorktreeDiskSettings";
import { WorktreeDiskPressure } from "./WorktreeDiskPressure";
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("../lib/worktreeDisk", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/worktreeDisk")>()),
  getWorktreeDisk: vi.fn(),
  saveWorktreeDiskSettings: vi.fn(),
}));
let root: Root;
let container: HTMLDivElement;
let events: Map<string, (event: { payload: unknown }) => void>;
function snapshot(): DiskSnapshot {
  return {
    schemaVersion: 1,
    settings: {
      schemaVersion: 1,
      version: 2,
      checkoutBudgetBytes: 30 * DISK_GIB,
      minimumFreeBytes: 10 * DISK_GIB,
      initialAllowanceBytes: 5 * DISK_GIB,
    },
    measuredAt: 10_000,
    complete: true,
    usedBytes: 12 * DISK_GIB,
    reclaimableBytes: 2 * DISK_GIB,
    pendingBytes: 3 * DISK_GIB,
    checkouts: [
      {
        id: "one",
        path: "/managed/one",
        projectCwd: "/project",
        estimatedBytes: 12 * DISK_GIB,
        accountedBytes: 12 * DISK_GIB,
        reclaimableBytes: 2 * DISK_GIB,
        missing: false,
        limitations: ["Contains shared hard links"],
        volumeIds: ["a"],
      },
      {
        id: "missing",
        path: "/managed/missing",
        projectCwd: "/project",
        estimatedBytes: 0,
        accountedBytes: 0,
        reclaimableBytes: 0,
        missing: true,
        limitations: [],
        volumeIds: [],
      },
    ],
    reservations: [
      {
        token: "one",
        path: "/managed/one",
        operation: "awaitingSetup",
        targetBytes: 15 * DISK_GIB,
        remainingBytes: 3 * DISK_GIB,
        createdAt: 1000,
        volumeIds: ["a"],
      },
    ],
    volumes: [
      {
        id: "a",
        path: "/managed",
        availableBytes: 50 * DISK_GIB,
        measuredAt: 10_000,
      },
    ],
    limitations: [
      "Symlinks are not followed.",
      "Filesystem clones prevent exact physical attribution.",
    ],
  };
}
beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  events = new Map();
  vi.mocked(listen).mockImplementation(async (name, handler) => {
    events.set(name, handler as (event: { payload: unknown }) => void);
    return vi.fn();
  });
  vi.mocked(getWorktreeDisk).mockResolvedValue(snapshot());
  vi.mocked(saveWorktreeDiskSettings).mockImplementation(async (settings) => ({
    ...settings,
    version: settings.version + 1,
  }));
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});
const button = (name: string) =>
  [...container.querySelectorAll("button")].find((b) =>
    b.textContent?.includes(name),
  )!;
async function enter(label: string, value: string) {
  const input = container.querySelector<HTMLInputElement>(
    `input[aria-label="${label}"]`,
  )!;
  await act(async () => {
    Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    )!.set!.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
  });
}
async function emit(name: string, payload: unknown) {
  await act(async () => events.get(name)!({ payload }));
}
it("shows usage, missing folders, reservations, measurement times and shared-byte limitations", async () => {
  await act(async () => root.render(createElement(WorktreeDiskSettings)));
  expect(container.textContent).toContain("12 GiB");
  expect(container.textContent).toContain("2 GiB");
  expect(container.textContent).toContain("50 GiB available");
  expect(container.textContent).toContain("Checkout measurement:");
  expect(container.textContent).toContain("Missing folder");
  expect(container.textContent).toContain("Waiting for setup");
  expect(container.textContent).toContain("shared hard links");
  expect(container.textContent).toContain("clones prevent exact");
  expect(container.textContent).toContain(
    "Configuration recovery storage has its own separate limit",
  );
  await act(async () => button("Measure now").click());
  expect(getWorktreeDisk).toHaveBeenLastCalledWith(true);
});
it("saves explicit budget/reserve disabling separately from the allowance", async () => {
  await act(async () => root.render(createElement(WorktreeDiskSettings)));
  const checkboxes = container.querySelectorAll<HTMLInputElement>(
    'input[type="checkbox"]',
  );
  await act(async () => {
    checkboxes[0].click();
    checkboxes[1].click();
  });
  await enter("Initial preparation allowance in GiB", "7.5");
  await act(async () => button("Save disk settings").click());
  expect(saveWorktreeDiskSettings).toHaveBeenCalledExactlyOnceWith({
    ...snapshot().settings,
    checkoutBudgetBytes: null,
    minimumFreeBytes: null,
    initialAllowanceBytes: 7.5 * DISK_GIB,
  });
  expect(container.textContent).toContain("Disk settings saved");
});
it("rejects zero and negative settings and keeps unsaved values on version conflicts", async () => {
  await act(async () => root.render(createElement(WorktreeDiskSettings)));
  await enter("Checkout budget in GiB", "0");
  expect(button("Save disk settings").disabled).toBe(true);
  await enter("Checkout budget in GiB", "-1");
  expect(button("Save disk settings").disabled).toBe(true);
  await enter("Checkout budget in GiB", "40");
  const newer = snapshot();
  newer.settings.version = 3;
  newer.settings.checkoutBudgetBytes = 60 * DISK_GIB;
  newer.measuredAt++;
  await emit("worktree-disk-snapshot", newer);
  expect(
    container.querySelector<HTMLInputElement>(
      'input[aria-label="Checkout budget in GiB"]',
    )!.value,
  ).toBe("40");
  expect(container.textContent).toContain("changed elsewhere");
  expect(button("Save disk settings").disabled).toBe(true);
  vi.mocked(getWorktreeDisk).mockResolvedValue(newer);
  await act(async () => button("Reload disk settings").click());
  expect(
    container.querySelector<HTMLInputElement>(
      'input[aria-label="Checkout budget in GiB"]',
    )!.value,
  ).toBe("60");
});
it("ignores stale measurements and exposes measurement errors without inventing zero usage", async () => {
  await act(async () => root.render(createElement(WorktreeDiskSettings)));
  const stale = snapshot();
  stale.measuredAt--;
  stale.usedBytes = 0;
  await emit("worktree-disk-snapshot", stale);
  expect(container.textContent).toContain("12 GiB");
  vi.mocked(getWorktreeDisk).mockRejectedValue("volume unavailable");
  await act(async () => button("Measure now").click());
  expect(container.textContent).toContain("volume unavailable");
  expect(container.textContent).toContain("12 GiB");
});
it("keeps pressure warnings visible until a newer sample clears pressure and offers cleanup/settings", async () => {
  const open = vi.fn();
  await act(async () =>
    root.render(createElement(WorktreeDiskPressure, { onOpenSettings: open })),
  );
  expect(container.querySelector("aside")).toBeNull();
  const s = snapshot();
  const low = {
    settings: s.settings,
    volumes: [
      { ...s.volumes[0], availableBytes: 5 * DISK_GIB, measuredAt: 40_000 },
    ],
    measuredAt: 40_000,
  };
  await emit("worktree-disk-pressure", low);
  expect(container.textContent).toContain("Low free space");
  expect(container.textContent).toContain("Ready workspaces remain usable");
  await emit("worktree-disk-snapshot", { ...s, measuredAt: 20_000 });
  expect(container.textContent).toContain("Low free space");
  await act(async () => {
    button("Review cleanup").click();
    button("Disk settings").click();
  });
  expect(open).toHaveBeenCalledTimes(2);
  await emit("worktree-disk-pressure", {
    settings: s.settings,
    volumes: [{ ...s.volumes[0], measuredAt: 70_000 }],
    measuredAt: 70_000,
  });
  expect(container.querySelector("aside")).toBeNull();
});
it("keeps the requested path in a capacity explanation until that workspace succeeds", async () => {
  await act(async () =>
    root.render(
      createElement(WorktreeDiskPressure, { onOpenSettings: vi.fn() }),
    ),
  );
  await act(async () =>
    window.dispatchEvent(
      new CustomEvent("worktree-capacity-failure", {
        detail: {
          code: "WORKTREE_CAPACITY",
          reason: "freeSpace",
          path: "/saved/worktree",
          operation: "restore",
          requiredBytes: 5 * DISK_GIB,
          volumeId: "a",
          snapshot: snapshot(),
          guidance: [],
        },
      }),
    ),
  );
  expect(container.textContent).toContain(
    "Requested workspace: /saved/worktree",
  );
  expect(container.textContent).toContain("explicitly reuse");
  await act(async () =>
    window.dispatchEvent(
      new CustomEvent("worktree-capacity-ready", { detail: "/other/worktree" }),
    ),
  );
  expect(container.textContent).toContain("/saved/worktree");
  await act(async () =>
    window.dispatchEvent(
      new CustomEvent("worktree-capacity-ready", {
        detail: "/saved/worktree/subdir",
      }),
    ),
  );
  expect(container.querySelector("aside")).toBeNull();
});

it("updates available space from 30-second probes without requesting another checkout scan", async () => {
  await act(async () => root.render(createElement(WorktreeDiskSettings)));
  const s = snapshot();
  await emit("worktree-disk-pressure", {
    settings: s.settings,
    measuredAt: 40_000,
    volumes: [
      { ...s.volumes[0], availableBytes: 4 * DISK_GIB, measuredAt: 40_000 },
    ],
  });
  expect(container.textContent).toContain("4 GiB available");
  expect(container.textContent).toContain("Low free space");
  expect(getWorktreeDisk).toHaveBeenCalledOnce();
  await emit("worktree-disk-snapshot", { ...s, measuredAt: 20_000 });
  expect(container.textContent).toContain("4 GiB available");
  expect(getWorktreeDisk).toHaveBeenCalledOnce();
});
