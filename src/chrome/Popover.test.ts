// @vitest-environment happy-dom
import { act, createElement } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { Popover } from "./Popover";

let root: Root;
let container: HTMLDivElement;
let anchor: HTMLButtonElement;
let anchorLeft: number;

beforeEach(() => {
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  // Browsers cannot focus the hidden measurement pass; happy-dom can.
  const focus = HTMLElement.prototype.focus;
  vi.spyOn(HTMLElement.prototype, "focus").mockImplementation(function (
    this: HTMLElement,
    options?: FocusOptions,
  ) {
    let element: HTMLElement | null = this;
    while (element) {
      if (element.style.visibility === "hidden") return;
      element = element.parentElement;
    }
    focus.call(this, options);
  });
  container = document.createElement("div");
  anchor = document.createElement("button");
  anchorLeft = 100;
  vi.spyOn(anchor, "getBoundingClientRect").mockImplementation(
    () => new DOMRect(anchorLeft, 100, 120, 30),
  );
  document.body.append(container, anchor);
  root = createRoot(container);
  anchor.focus();
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  anchor.remove();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("focuses after placement and preserves descendant focus when repositioning", async () => {
  await act(async () => {
    root.render(
      createElement(
        Popover,
        { anchor, autoFocus: true, tabIndex: -1, role: "listbox", width: 240 },
        createElement("button", { type: "button" }, "Focused action"),
      ),
    );
  });

  const surface = document.querySelector<HTMLElement>('[role="listbox"]')!;
  expect(surface.parentElement!.style.visibility).not.toBe("hidden");
  expect(document.activeElement).toBe(surface);

  const action = surface.querySelector("button")!;
  action.focus();
  const previousLeft = surface.parentElement!.style.left;
  await act(async () => {
    anchorLeft = 200;
    window.dispatchEvent(new Event("resize"));
  });
  expect(surface.parentElement!.style.left).not.toBe(previousLeft);
  expect(document.activeElement).toBe(action);

  await act(async () => {
    anchorLeft = 300;
    window.dispatchEvent(new Event("scroll"));
  });
  expect(document.activeElement).toBe(action);
});
