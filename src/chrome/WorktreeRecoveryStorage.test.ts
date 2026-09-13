// @vitest-environment happy-dom
import { listen } from "@tauri-apps/api/event";
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  getRecoveryStorage,
  RECOVERY_STORAGE_MIB,
  setRecoveryStorageLimit,
  type RecoveryStorageUsage,
} from "../lib/worktreeStorage";
import { WorktreeRecoveryStorage } from "./WorktreeRecoveryStorage";

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
vi.mock("../lib/worktreeStorage", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/worktreeStorage")>()),
  getRecoveryStorage: vi.fn(),
  setRecoveryStorageLimit: vi.fn(),
}));

let root: Root;
let container: HTMLDivElement;

function storage(
  usedMiB = 32,
  limitMiB = 64,
  version = 3,
): RecoveryStorageUsage {
  return {
    usedBytes: usedMiB * RECOVERY_STORAGE_MIB,
    limitBytes: limitMiB * RECOVERY_STORAGE_MIB,
    version,
    projects: [
      {
        projectCwd: "/repo/apps/web/",
        usedBytes: 12.5 * RECOVERY_STORAGE_MIB,
      },
      {
        projectCwd: "/repo/apps/api",
        usedBytes: 24 * RECOVERY_STORAGE_MIB,
      },
    ],
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.mocked(listen).mockResolvedValue(vi.fn());
  vi.mocked(getRecoveryStorage).mockResolvedValue(storage());
  vi.mocked(setRecoveryStorageLimit).mockImplementation(
    async (limitBytes, expectedVersion) => ({
      ...storage(),
      limitBytes,
      version: expectedVersion + 1,
    }),
  );
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

async function render(projectCwd = "/repo/apps/web") {
  await act(async () => {
    root.render(createElement(WorktreeRecoveryStorage, { projectCwd }));
  });
}

async function openStorage() {
  const summary = container.querySelector("summary");
  expect(summary).not.toBeNull();
  await act(async () => summary!.click());
}

function button(text: string): HTMLButtonElement {
  return [...container.querySelectorAll<HTMLButtonElement>("button")].find(
    (candidate) => candidate.textContent?.includes(text),
  )!;
}

function limitInput(): HTMLInputElement {
  return container.querySelector<HTMLInputElement>(
    'input[aria-label="Recovery storage limit in MB"]',
  )!;
}

async function enterLimit(value: string) {
  const element = limitInput();
  await act(async () => {
    Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    )?.set?.call(element, value);
    element.dispatchEvent(new Event("input", { bubbles: true }));
  });
}

describe("WorktreeRecoveryStorage", () => {
  it("marks 80 percent usage as near the limit and shows selected-project usage", async () => {
    vi.mocked(getRecoveryStorage).mockResolvedValue(storage(80, 100));
    await render("/repo/apps/web");

    const summary = container.querySelector("summary")!;
    expect(summary.textContent).toContain("80 MB of 100 MB used app-wide");
    expect(summary.textContent).toContain("Near limit");
    expect(summary.querySelector(".text-amber-400")).not.toBeNull();

    await openStorage();
    expect(container.textContent).toContain("This project: 12.5 MB");
    expect(container.textContent).toContain("Project totals may overlap");
  });

  it("marks usage at the limit as full", async () => {
    vi.mocked(getRecoveryStorage).mockResolvedValue(storage(100, 100));
    await render();

    const summary = container.querySelector("summary")!;
    expect(summary.textContent).toContain("Storage full");
    expect(summary.querySelector(".text-red-400")).not.toBeNull();
  });

  it("saves an integer MB limit as bytes with the loaded revision", async () => {
    vi.mocked(getRecoveryStorage).mockResolvedValue(storage(32, 64, 7));
    await render();
    await openStorage();
    await enterLimit("96");
    await act(async () => button("Save limit").click());

    expect(setRecoveryStorageLimit).toHaveBeenCalledWith(
      96 * RECOVERY_STORAGE_MIB,
      7,
    );
    expect(limitInput().value).toBe("96");
    expect(container.querySelector('[role="status"]')?.textContent).toContain(
      "saved",
    );
  });

  it("preserves a stale draft across refresh until saved data is explicitly reloaded", async () => {
    vi.mocked(setRecoveryStorageLimit).mockRejectedValueOnce(
      "WORKTREE_STORAGE_CONFLICT: storage settings changed",
    );
    await render();
    await openStorage();
    await enterLimit("96");
    await act(async () => button("Save limit").click());

    expect(limitInput().value).toBe("96");
    expect(container.querySelector('[role="alert"]')?.textContent).toContain(
      "changed elsewhere",
    );

    vi.mocked(getRecoveryStorage).mockResolvedValue(storage(34, 80, 4));
    await act(async () => window.dispatchEvent(new Event("focus")));
    expect(limitInput().value).toBe("96");
    expect(button("Save limit").disabled).toBe(true);

    await act(async () => button("Reload saved limit").click());
    expect(limitInput().value).toBe("80");
    expect(button("Save limit").disabled).toBe(true);
  });

  it("does not let a pre-save refresh restore an older limit or revision", async () => {
    let finishRefresh!: (value: RecoveryStorageUsage) => void;
    await render();
    vi.mocked(getRecoveryStorage).mockReturnValueOnce(
      new Promise((resolve) => {
        finishRefresh = resolve;
      }),
    );
    act(() => window.dispatchEvent(new Event("focus")));
    await openStorage();
    await enterLimit("96");
    await act(async () => button("Save limit").click());

    await act(async () => finishRefresh(storage(31, 64, 3)));
    expect(limitInput().value).toBe("96");
    expect(container.querySelector("summary")?.textContent).toContain(
      "of 96 MB used app-wide",
    );

    await enterLimit("100");
    await act(async () => button("Save limit").click());
    expect(setRecoveryStorageLimit).toHaveBeenNthCalledWith(
      2,
      100 * RECOVERY_STORAGE_MIB,
      4,
    );
  });

  it("keeps a newer observed version when it arrives before a save response", async () => {
    let finishSave!: (value: RecoveryStorageUsage) => void;
    vi.mocked(setRecoveryStorageLimit).mockReturnValueOnce(
      new Promise((resolve) => {
        finishSave = resolve;
      }),
    );
    await render();
    await openStorage();
    await enterLimit("96");
    act(() => button("Save limit").click());

    vi.mocked(getRecoveryStorage).mockResolvedValue(storage(34, 80, 5));
    await act(async () => window.dispatchEvent(new Event("focus")));
    await act(async () => finishSave(storage(33, 96, 4)));

    expect(limitInput().value).toBe("96");
    expect(container.querySelector("summary")?.textContent).toContain(
      "of 80 MB used app-wide",
    );
    expect(container.querySelector('[role="alert"]')?.textContent).toContain(
      "changed elsewhere",
    );
    expect(button("Save limit").disabled).toBe(true);
  });

  it("reports a read error inline and retries without affecting its parent", async () => {
    vi.mocked(getRecoveryStorage)
      .mockRejectedValueOnce(new Error("Storage index unavailable"))
      .mockResolvedValue(storage());
    await render();
    await openStorage();

    expect(container.querySelector('[role="alert"]')?.textContent).toContain(
      "Storage index unavailable",
    );
    await act(async () => button("Try again").click());
    expect(container.querySelector("summary")?.textContent).toContain(
      "32 MB of 64 MB used app-wide",
    );
    expect(getRecoveryStorage).toHaveBeenCalledTimes(2);
  });
});
