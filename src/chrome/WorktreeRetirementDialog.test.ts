// @vitest-environment happy-dom
import { act, createElement, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { WorktreeRetirementDialog } from "./WorktreeRetirementDialog";
import { retireWorktrees, type WorktreeRetirementPlan } from "../lib/worktrees";

vi.mock("../lib/worktrees", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/worktrees")>()),
  retireWorktrees: vi.fn(),
}));

let root: Root;
let container: HTMLDivElement;

function plan(ids = ["alpha"]): WorktreeRetirementPlan {
  return {
    planId: "plan-7",
    entries: ids.map((id) => ({
      id,
      repo: "/Users/demo/Projects/monocode",
      path: `/Users/demo/Projects/monocode/.worktrees/${id}`,
      branch: `feature/${id}`,
      blockedReason: null,
      localBranch: {
        name: `feature/${id}`,
        allowed: true,
        reason: null,
      },
      remoteBranch:
        id === "beta"
          ? null
          : {
              name: `feature/${id}`,
              remote: "upstream",
              destination: `github.com/acme/monocode/${id}`,
              allowed: true,
              reason: null,
            },
    })),
    kept: [],
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal("IS_REACT_ACT_ENVIRONMENT", true);
  container = document.createElement("div");
  document.body.append(container);
  root = createRoot(container);
});

afterEach(() => {
  act(() => root.unmount());
  container.remove();
  vi.unstubAllGlobals();
});

async function render(
  props: Partial<ComponentProps<typeof WorktreeRetirementDialog>> = {},
) {
  const defaults = {
    plan: plan(),
    source: "archive" as const,
    onClose: vi.fn(),
    onRetired: vi.fn(),
  };
  const merged = { ...defaults, ...props };
  await act(async () =>
    root.render(createElement(WorktreeRetirementDialog, merged)),
  );
  return merged;
}

function button(text: string): HTMLButtonElement {
  return [...document.querySelectorAll<HTMLButtonElement>("button")].find(
    (candidate) => candidate.textContent?.includes(text),
  )!;
}

function checkbox(label: string, name: string): HTMLInputElement {
  const match = [...document.querySelectorAll<HTMLLabelElement>("label")].find(
    (candidate) =>
      candidate.textContent?.includes(label) &&
      candidate.textContent?.includes(name),
  );
  return match!.querySelector("input")!;
}

describe("WorktreeRetirementDialog", () => {
  it("keeps keyboard focus in the review and restores its opener", async () => {
    const opener = document.createElement("button");
    opener.textContent = "Archive";
    document.body.append(opener);
    opener.focus();
    await render();

    expect(document.activeElement?.textContent).toContain("Keep for now");
    const retire = button("Retire worktree");
    retire.focus();
    await act(async () =>
      retire.dispatchEvent(
        new KeyboardEvent("keydown", { key: "Tab", bubbles: true }),
      ),
    );
    const close = document.querySelector<HTMLButtonElement>(
      'button[aria-label="Close"]',
    )!;
    expect(document.activeElement).toBe(close);
    await act(async () =>
      close.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "Tab",
          shiftKey: true,
          bubbles: true,
        }),
      ),
    );
    expect(document.activeElement).toBe(retire);

    await act(async () => root.render(null));
    expect(document.activeElement).toBe(opener);
    opener.remove();
  });

  it("keeps branches by default and shows exact local and remote destinations", async () => {
    const onClose = vi.fn();
    await render({ onClose });

    expect(document.querySelector('[role="dialog"]')?.textContent).toContain(
      "The conversation is archived",
    );
    expect(document.body.textContent).toContain("feature/alpha");
    expect(document.body.textContent).toContain(
      "upstream → github.com/acme/monocode/alpha",
    );
    expect(document.body.textContent).toContain(
      "Monocode preserves your committed code and configured local files",
    );
    expect(document.body.textContent).toContain(
      "Branches stay unless you select them below",
    );
    expect(document.body.textContent).not.toContain("recovery ref");
    expect(checkbox("Delete local branch", "feature/alpha").checked).toBe(
      false,
    );
    expect(checkbox("Delete remote branch", "feature/alpha").checked).toBe(
      false,
    );

    await act(async () => button("Keep for now").click());
    expect(onClose).toHaveBeenCalledOnce();
    expect(retireWorktrees).not.toHaveBeenCalled();
  });

  it("explains disabled choices and worktrees kept for safety", async () => {
    const blockedPlan = plan();
    blockedPlan.entries[0].localBranch.allowed = false;
    blockedPlan.entries[0].localBranch.reason =
      "This branch is used by another working folder";
    blockedPlan.entries[0].remoteBranch!.allowed = false;
    blockedPlan.entries[0].remoteBranch!.reason =
      "The remote destination could not be verified";
    blockedPlan.kept = [
      {
        id: "pinned",
        path: "/Users/demo/Projects/monocode/.worktrees/pinned",
        reason: "Pinned",
      },
    ];
    await render({ plan: blockedPlan });

    expect(checkbox("Delete local branch", "feature/alpha").disabled).toBe(
      true,
    );
    expect(checkbox("Delete remote branch", "feature/alpha").disabled).toBe(
      true,
    );
    expect(document.body.textContent).toContain(
      "This branch is used by another working folder",
    );
    expect(document.body.textContent).toContain(
      "The remote destination could not be verified",
    );
    expect(document.body.textContent).toContain("Kept for safety");
    expect(document.body.textContent).toContain("Pinned");
  });

  it("labels a journaled branch-only retry without promising another folder removal", async () => {
    const pendingPlan = plan();
    pendingPlan.entries[0].worktreeRemoved = true;
    await render({ plan: pendingPlan, source: "settings" });

    expect(document.body.textContent).toContain(
      "The working folder is already removed",
    );
    expect(document.body.textContent).toContain(
      "Working folder already removed. Finish any branch removal below.",
    );
    expect(document.body.textContent).not.toContain(
      "The working folder will be removed",
    );
    expect(checkbox("Delete local branch", "feature/alpha").checked).toBe(
      false,
    );
    expect(checkbox("Delete remote branch", "feature/alpha").checked).toBe(
      false,
    );
  });

  it("sends one bulk confirmation with every branch choice", async () => {
    const bulkPlan = plan(["alpha", "beta"]);
    let finish!: (value: { results: [] }) => void;
    vi.mocked(retireWorktrees).mockReturnValue(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    await render({ plan: bulkPlan, source: "settings" });
    await act(async () =>
      checkbox("Delete local branch", "feature/alpha").click(),
    );
    await act(async () =>
      checkbox("Delete remote branch", "feature/alpha").click(),
    );
    await act(async () =>
      checkbox("Delete local branch", "feature/beta").click(),
    );
    await act(async () => button("Retire 2 worktrees").click());

    expect(retireWorktrees).toHaveBeenCalledOnce();
    expect(retireWorktrees).toHaveBeenCalledWith("plan-7", [
      {
        id: "alpha",
        deleteLocalBranch: true,
        deleteRemoteBranch: true,
      },
      {
        id: "beta",
        deleteLocalBranch: true,
        deleteRemoteBranch: false,
      },
    ]);
    expect(button("Retire 2 worktrees").disabled).toBe(true);
    expect(
      document.querySelector<HTMLButtonElement>('button[aria-label="Close"]')
        ?.disabled,
    ).toBe(true);
    expect(document.querySelector('[role="status"]')?.textContent).toContain(
      "Retiring worktrees",
    );
    await act(async () => finish({ results: [] }));
  });

  it("shows a partial result and retries only the unfinished deletion", async () => {
    const onRetired = vi.fn();
    vi.mocked(retireWorktrees)
      .mockResolvedValueOnce({
        results: [
          {
            id: "alpha",
            path: "/Users/demo/Projects/monocode/.worktrees/alpha",
            worktreeRemoved: true,
            localBranchDeleted: true,
            remoteBranchDeleted: false,
            recoveryRef: "monocode/recovery/alpha",
            error: "The remote server could not be reached",
          },
        ],
      })
      .mockResolvedValueOnce({
        results: [
          {
            id: "alpha",
            path: "/Users/demo/Projects/monocode/.worktrees/alpha",
            worktreeRemoved: true,
            localBranchDeleted: true,
            remoteBranchDeleted: true,
            recoveryRef: "monocode/recovery/alpha",
            error: null,
          },
        ],
      });
    await render({ onRetired });
    await act(async () =>
      checkbox("Delete local branch", "feature/alpha").click(),
    );
    await act(async () =>
      checkbox("Delete remote branch", "feature/alpha").click(),
    );
    await act(async () => button("Retire worktree").click());

    expect(document.body.textContent).toContain("Working folder removed");
    expect(document.body.textContent).toContain("Local branch deleted");
    expect(document.body.textContent).toContain(
      "Remote deletion not confirmed",
    );
    expect(document.body.textContent).toContain(
      "Reopen the archived conversation to restore its configured local files",
    );
    expect(document.body.textContent).not.toContain("monocode/recovery/alpha");
    expect(document.body.textContent).toContain(
      "The remote server could not be reached",
    );
    expect(button("Retry unfinished work")).toBeTruthy();

    await act(async () => button("Retry unfinished work").click());
    expect(retireWorktrees).toHaveBeenLastCalledWith("plan-7", [
      {
        id: "alpha",
        deleteLocalBranch: false,
        deleteRemoteBranch: true,
      },
    ]);
    expect(document.body.textContent).toContain("Remote branch deleted");
    expect(button("Retry unfinished work")).toBeUndefined();
    expect(onRetired).toHaveBeenCalledTimes(2);
    expect(onRetired).toHaveBeenLastCalledWith({
      results: [
        expect.objectContaining({
          id: "alpha",
          worktreeRemoved: true,
          localBranchDeleted: true,
          remoteBranchDeleted: true,
          error: null,
        }),
      ],
    });
  });

  it("replaces prior success with the current state when a checkout was restored before retry", async () => {
    const result = {
      id: "alpha",
      path: "/worktrees/alpha",
      worktreeRemoved: true,
      localBranchDeleted: true,
      remoteBranchDeleted: false,
      recoveryRef: "refs/monocode/recovery/alpha",
      error: "Remote unavailable",
    };
    vi.mocked(retireWorktrees)
      .mockResolvedValueOnce({ results: [result] })
      .mockResolvedValueOnce({
        results: [
          {
            ...result,
            worktreeRemoved: false,
            localBranchDeleted: false,
            error: "Worktree was restored; review it again",
          },
        ],
      });
    await render();
    await act(async () =>
      checkbox("Delete local branch", "feature/alpha").click(),
    );
    await act(async () =>
      checkbox("Delete remote branch", "feature/alpha").click(),
    );
    await act(async () => button("Retire worktree").click());
    await act(async () => button("Retry unfinished work").click());
    expect(document.body.textContent).toContain("Working folder kept");
    expect(document.body.textContent).toContain("Local deletion not confirmed");
    expect(document.body.textContent).not.toContain("Working folder removed");
    expect(document.body.textContent).not.toContain("Local branch deleted");
  });

  it("says files remain in the checkout when recovery capacity keeps it", async () => {
    vi.mocked(retireWorktrees).mockResolvedValue({
      results: [
        {
          id: "alpha",
          path: "/Users/demo/Projects/monocode/.worktrees/alpha",
          worktreeRemoved: false,
          localBranchDeleted: false,
          remoteBranchDeleted: false,
          recoveryRef: "monocode/recovery/alpha",
          error: "Recovery storage is full. Increase its limit and try again.",
        },
      ],
    });
    await render();
    await act(async () => button("Retire worktree").click());

    expect(document.body.textContent).toContain("Working folder kept");
    expect(document.body.textContent).toContain("Files remain in the checkout");
    expect(document.body.textContent).not.toContain(
      "restore its configured local files",
    );
    expect(document.body.textContent).not.toContain("run setup again");
    expect(document.body.textContent).toContain("Recovery storage is full");
  });

  it("shows a request failure and leaves confirmation enabled", async () => {
    vi.mocked(retireWorktrees).mockRejectedValue(
      new Error("The retirement plan expired"),
    );
    await render();
    await act(async () => button("Retire worktree").click());

    expect(document.querySelector('[role="alert"]')?.textContent).toContain(
      "The retirement plan expired",
    );
    expect(button("Retire worktree").disabled).toBe(false);
  });

  it("does not offer retirement when every planned entry is blocked", async () => {
    const blockedPlan = plan();
    blockedPlan.entries[0].blockedReason = "Contains uncommitted changes";
    await render({ plan: blockedPlan });

    expect(document.body.textContent).toContain(
      "Kept for safety: Contains uncommitted changes",
    );
    expect(button("Retire worktree")).toBeUndefined();
    expect(button("Keep for now")).toBeTruthy();
  });
});
