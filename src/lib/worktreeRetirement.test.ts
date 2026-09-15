import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { archiveSessionsWithRetirement } from "./worktreeRetirement";
import { heartbeatWorktrees, type WorktreeRetirementPlan } from "./worktrees";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const emptyPlan: WorktreeRetirementPlan = {
  planId: "review",
  entries: [],
  kept: [],
};

beforeEach(() => {
  vi.mocked(invoke).mockReset();
});

function fixture(sessionIds: string[]) {
  const order: string[] = [];
  let paths = ["/repo/checkout"];
  const archive = vi.fn(async (id: string) => {
    order.push(`archive:${id}`);
    paths = ["/repo"];
    return true;
  });
  vi.mocked(invoke).mockImplementation(async (command) => {
    order.push(command);
    return (
      command === "worktree_archive_retirement"
        ? { review: emptyPlan, automatic: [] }
        : undefined
    ) as never;
  });
  const options = {
    sessionIds,
    archive,
    protectedPaths: () => paths,
    onReview: vi.fn(),
    onReviewError: vi.fn(),
    onAutomatic: vi.fn(),
  };
  return { order, options, run: () => archiveSessionsWithRetirement(options) };
}

describe("archive retirement boundary", () => {
  it("reports durable automatic failures without changing archive success or asking to delete branches", async () => {
    const task = fixture(["first", "second"]);
    const automatic = [
      {
        id: "checkout",
        path: "/managed/checkout",
        status: "failed",
        planId: "existing-plan",
        reason: "Recovery storage is full",
        updatedAt: 100,
      },
    ];
    vi.mocked(invoke).mockImplementation(
      async (command) =>
        (command === "worktree_archive_retirement"
          ? { review: emptyPlan, automatic }
          : undefined) as never,
    );
    expect(await task.run()).toBe(true);
    expect(task.options.onAutomatic).toHaveBeenCalledExactlyOnceWith(automatic);
    expect(task.options.onReviewError).not.toHaveBeenCalled();
    expect(invoke).not.toHaveBeenCalledWith(
      "worktree_retire",
      expect.anything(),
    );
  });

  it("can deliver one manual review and automatic completion from a mixed-project batch", async () => {
    const task = fixture(["manual", "automatic"]);
    const review = {
      ...emptyPlan,
      kept: [{ id: "manual-checkout", path: "/manual", reason: "Pinned" }],
    };
    const automatic = [
      {
        id: "automatic-checkout",
        path: "/automatic",
        status: "complete",
        planId: "auto-plan",
        reason: null,
        updatedAt: 100,
      },
    ];
    vi.mocked(invoke).mockImplementation(
      async (command) =>
        (command === "worktree_archive_retirement"
          ? { review, automatic }
          : undefined) as never,
    );
    expect(await task.run()).toBe(true);
    expect(task.options.onReview).toHaveBeenCalledExactlyOnceWith(review);
    expect(task.options.onAutomatic).toHaveBeenCalledExactlyOnceWith(automatic);
  });
  it("commits the whole batch before releasing leases and making one review", async () => {
    const task = fixture(["first", "second", "first"]);
    expect(await task.run()).toBe(true);
    expect(task.order).toEqual([
      "archive:first",
      "archive:second",
      "worktree_heartbeat",
      "worktree_archive_retirement",
    ]);
    expect(invoke).toHaveBeenCalledWith("worktree_heartbeat", {
      paths: ["/repo"],
    });
    expect(invoke).toHaveBeenCalledWith("worktree_archive_retirement", {
      sessionIds: ["first", "second"],
    });
    expect(task.options.onReview).toHaveBeenCalledExactlyOnceWith(emptyPlan);
  });

  it("reviews only completed archives when a later archive is cancelled", async () => {
    const task = fixture(["first", "cancelled", "untouched"]);
    task.options.archive
      .mockResolvedValueOnce(true)
      .mockResolvedValueOnce(false);
    expect(await task.run()).toBe(false);
    expect(task.options.archive.mock.calls).toEqual([["first"], ["cancelled"]]);
    expect(invoke).toHaveBeenLastCalledWith("worktree_archive_retirement", {
      sessionIds: ["first"],
    });
  });

  it("does not request cleanup when no session was archived", async () => {
    const task = fixture(["cancelled"]);
    task.options.archive.mockResolvedValue(false);
    expect(await task.run()).toBe(false);
    expect(invoke).not.toHaveBeenCalled();
    expect(task.options.onReview).not.toHaveBeenCalled();
  });

  it("keeps archive successful when planning fails", async () => {
    const task = fixture(["first"]);
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "worktree_archive_retirement")
        throw new Error("Repository unavailable");
      return undefined as never;
    });
    expect(await task.run()).toBe(true);
    expect(task.options.onReviewError).toHaveBeenCalledExactlyOnceWith(
      expect.objectContaining({ message: "Repository unavailable" }),
    );
    expect(task.options.onReview).not.toHaveBeenCalled();
  });

  it("cannot review until the lease release succeeds", async () => {
    const task = fixture(["first"]);
    vi.mocked(invoke).mockRejectedValue(new Error("Window lease unavailable"));
    expect(await task.run()).toBe(true);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(task.options.onReviewError).toHaveBeenCalledTimes(1);
    expect(task.options.onReview).not.toHaveBeenCalled();
  });

  it("still reviews earlier archives if a later archive throws", async () => {
    const task = fixture(["first", "failed"]);
    task.options.archive
      .mockResolvedValueOnce(true)
      .mockRejectedValueOnce(new Error("Storage failed"));
    await expect(task.run()).rejects.toThrow("Storage failed");
    expect(invoke).toHaveBeenLastCalledWith("worktree_archive_retirement", {
      sessionIds: ["first"],
    });
  });
});

describe("worktree window leases", () => {
  it("serializes heartbeats so an older lease cannot arrive after its release", async () => {
    let release!: () => void;
    vi.mocked(invoke)
      .mockImplementationOnce(
        () =>
          new Promise<void>((resolve) => {
            release = resolve;
          }) as never,
      )
      .mockResolvedValue(undefined);
    const old = heartbeatWorktrees(["/checkout"]);
    const current = heartbeatWorktrees([]);
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledTimes(1));
    release();
    await Promise.all([old, current]);
    expect(vi.mocked(invoke).mock.calls).toEqual([
      ["worktree_heartbeat", { paths: ["/checkout"] }],
      ["worktree_heartbeat", { paths: [] }],
    ]);
  });

  it("a failed heartbeat does not prevent subsequent leases from updating", async () => {
    vi.mocked(invoke)
      .mockRejectedValueOnce(new Error("temporary"))
      .mockResolvedValue(undefined);
    await expect(heartbeatWorktrees(["/checkout"])).rejects.toThrow(
      "temporary",
    );
    await expect(heartbeatWorktrees([])).resolves.toBeUndefined();
    expect(invoke).toHaveBeenLastCalledWith("worktree_heartbeat", {
      paths: [],
    });
  });
});
