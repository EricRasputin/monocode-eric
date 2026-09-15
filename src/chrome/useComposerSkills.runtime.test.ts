// @vitest-environment happy-dom
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useComposerSkills } from "./useComposerSkills";
import { loadSkills } from "../lib/skills";

vi.mock("../lib/skills", () => ({
  loadSkills: vi.fn(async () => []),
  hasNativeCommands: () => true,
  mergeCatalog: () => [],
  peekSkills: () => null,
  subscribeSkills: () => () => {},
  skillCatalogKey: ({ cwd }: { cwd: string }) => cwd,
  SKILLS_CHANGE_EVENT: "skills-changed",
}));

let root: Root;
beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  root = createRoot(document.createElement("div"));
});
afterEach(() => { act(() => root.unmount()); vi.unstubAllGlobals(); });

it("does not discover provider skills while viewing history; preparation enables discovery", async () => {
  function ComposerSkills({ enabled }: { enabled: boolean }) {
    useComposerSkills({ harness: "pi", executionCwd: "/saved-worktree", sessionId: "saved", enabled, pickerOpen: false });
    return null;
  }
  await act(async () => root.render(createElement(ComposerSkills, { enabled: false })));
  act(() => window.dispatchEvent(new Event("skills-changed")));
  expect(loadSkills).not.toHaveBeenCalled();
  await act(async () => root.render(createElement(ComposerSkills, { enabled: true })));
  expect(loadSkills).toHaveBeenCalledWith({ harness: "pi", cwd: "/saved-worktree", sessionId: "saved" }, undefined);
});
