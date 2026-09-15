// @vitest-environment happy-dom
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useComposerFiles } from "./useComposerFiles";
import {
  loadProjectFiles,
  peekProjectFiles,
  subscribeProjectFiles,
} from "../lib/fileIndex";
import type { ProjectFile } from "../lib/fs";

vi.mock("../lib/fileIndex", () => ({
  loadProjectFiles: vi.fn(),
  peekProjectFiles: vi.fn(),
  subscribeProjectFiles: vi.fn(),
}));
let root: Root;
let container: HTMLDivElement;
let notify: () => void;
const cache = new Map<string, ProjectFile[]>();
const file = (cwd: string, name: string): ProjectFile => ({
  path: `${cwd}/${name}`,
  relative: name,
});
function Files({ cwd, enabled = true }: { cwd: string; enabled?: boolean }) {
  return createElement(
    "div",
    null,
    useComposerFiles(cwd, enabled, false)
      .map((file) => file.path)
      .join(","),
  );
}
beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  cache.clear();
  cache.set("/main", [file("/main", "main-only.ts")]);
  cache.set("/worktree", [file("/worktree", "worktree-only.ts")]);
  vi.mocked(peekProjectFiles).mockImplementation(
    (cwd) => cache.get(cwd) ?? null,
  );
  vi.mocked(loadProjectFiles).mockImplementation(
    async (cwd) => cache.get(cwd) ?? [],
  );
  vi.mocked(subscribeProjectFiles).mockImplementation((listener) => {
    notify = listener;
    return () => {};
  });
  container = document.createElement("div");
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  vi.unstubAllGlobals();
});

it("uses the execution checkout for initial files and subscription updates", async () => {
  await act(async () =>
    root.render(createElement(Files, { cwd: "/worktree" })),
  );
  expect(container.textContent).toBe("/worktree/worktree-only.ts");
  expect(loadProjectFiles).toHaveBeenCalledWith("/worktree", false);
  cache.set("/worktree", [file("/worktree", "changed.ts")]);
  act(() => notify());
  expect(container.textContent).toBe("/worktree/changed.ts");
  expect(peekProjectFiles).not.toHaveBeenCalledWith("/main");
});

it("does not retain primary files when switching to an uncached execution checkout", async () => {
  await act(async () => root.render(createElement(Files, { cwd: "/main" })));
  expect(container.textContent).toContain("main-only.ts");
  vi.mocked(loadProjectFiles).mockImplementation(() => new Promise(() => {}));
  await act(async () =>
    root.render(createElement(Files, { cwd: "/new-worktree" })),
  );
  expect(container.textContent).toBe("");
});

it("does not read or subscribe to workspace files for transcript-only history", async () => {
  await act(async () =>
    root.render(createElement(Files, { cwd: "/worktree", enabled: false })),
  );
  expect(container.textContent).toBe("");
  expect(loadProjectFiles).not.toHaveBeenCalled();
  expect(subscribeProjectFiles).not.toHaveBeenCalled();
  await act(async () =>
    root.render(createElement(Files, { cwd: "/worktree", enabled: true })),
  );
  expect(container.textContent).toContain("worktree-only.ts");
});
