// @vitest-environment happy-dom
import { act, createElement, useEffect } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { WorkspaceAccess, WorkspacePreparation } from "./WorkspaceAccess";

let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  container = document.createElement("div");
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  vi.unstubAllGlobals();
});

it.each(["editor read", "terminal spawn"])(
  "gates a reloaded %s, keeps failures retryable, and uses the prepared directory",
  async () => {
    let finish!: (cwd: string) => void;
    const prepare = vi
      .fn()
      .mockRejectedValueOnce(new Error("Setup failed"))
      .mockImplementation(
        () =>
          new Promise<string>((resolve) => {
            finish = resolve;
          }),
      );
    const useFilesystem = vi.fn();
    function Surface({ cwd }: { cwd: string }) {
      useEffect(() => {
        useFilesystem(cwd);
      }, [cwd]);
      return createElement("div", null, cwd);
    }
    await act(async () =>
      root.render(
        createElement(WorkspacePreparation.Provider, {
          value: prepare,
          children: createElement(WorkspaceAccess, {
            cwd: "/saved",
            path: "/saved/file.ts",
            children: (cwd) => createElement(Surface, { cwd }),
          }),
        }),
      ),
    );
    expect(container.textContent).toContain("Setup failed");
    expect(useFilesystem).not.toHaveBeenCalled();
    await act(async () => container.querySelector("button")!.click());
    expect(useFilesystem).not.toHaveBeenCalled();
    await act(async () => finish("/prepared"));
    expect(useFilesystem).toHaveBeenCalledExactlyOnceWith("/prepared");
    expect(prepare).toHaveBeenCalledTimes(2);
    expect(prepare).toHaveBeenLastCalledWith("/saved", "/saved/file.ts");
  },
);
