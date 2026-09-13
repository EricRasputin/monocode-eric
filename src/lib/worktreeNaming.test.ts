import { beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import { finishWorktreeNaming, type WorktreeNaming } from "./worktreeNaming";
import { parseGeneratedSessionTitle } from "./sessionTitle";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("sonner", () => ({
  toast: {
    message: vi.fn(),
    loading: vi.fn(),
    success: vi.fn(),
    dismiss: vi.fn(),
  },
}));

beforeEach(() => vi.resetAllMocks());

const request = { sessionId: "session", token: "original-request" };
const job = (overrides: Partial<WorktreeNaming> = {}): WorktreeNaming => ({
  token: request.token,
  result: Promise.resolve(null),
  retry: vi.fn().mockResolvedValue("add-search"),
  isCurrent: () => true,
  ...overrides,
});

function clickRetry() {
  const options = vi.mocked(toast.message).mock.calls.at(-1)?.[1];
  const action = options?.action;
  expect(action).toMatchObject({ label: "Retry" });
  if (action && typeof action === "object" && "onClick" in action) {
    const preventDefault = vi.fn();
    action.onClick({ preventDefault } as never);
    expect(preventDefault).toHaveBeenCalledOnce();
  }
}

describe("worktree naming feedback and retry", () => {
  it("rejects quota text, keeps the fallback and retries the original request", async () => {
    const rejected = parseGeneratedSessionTitle(
      "Upgrade your plan to continue",
      "Add search",
    );
    const naming = job({ result: Promise.resolve(rejected?.branch ?? null) });
    vi.mocked(invoke)
      .mockResolvedValueOnce("waiting")
      .mockResolvedValueOnce("waiting")
      .mockResolvedValueOnce("named");
    await finishWorktreeNaming("session", naming, Promise.resolve());

    expect(invoke).toHaveBeenCalledExactlyOnceWith(
      "worktree_name_status",
      request,
    );
    expect(toast.message).toHaveBeenCalledWith(
      "AI naming unavailable",
      expect.objectContaining({
        duration: Infinity,
        action: expect.any(Object),
      }),
    );
    expect(naming.retry).not.toHaveBeenCalled();
    clickRetry();
    await vi.waitFor(() => expect(toast.success).toHaveBeenCalled());
    expect(naming.retry).toHaveBeenCalledOnce();
    expect(invoke).toHaveBeenLastCalledWith("worktree_name", {
      ...request,
      branch: "add-search",
    });
  });

  it("does not fall back to a title when the structured branch is missing", async () => {
    const metadata = parseGeneratedSessionTitle(
      '{"title":"Add search"}',
      "Add search",
    );
    vi.mocked(invoke).mockResolvedValue("waiting");
    await finishWorktreeNaming(
      "session",
      job({ result: Promise.resolve(metadata?.branch ?? null) }),
      Promise.resolve(),
    );
    expect(invoke).not.toHaveBeenCalledWith("worktree_name", expect.anything());
    expect(toast.message).toHaveBeenCalled();
  });

  it("deduplicates repeated Retry clicks and permits another retry after failure", async () => {
    let finish!: (branch: string | null) => void;
    const retry = vi
      .fn()
      .mockImplementationOnce(
        () =>
          new Promise<string | null>((resolve) => {
            finish = resolve;
          }),
      )
      .mockResolvedValue("add-search");
    vi.mocked(invoke).mockResolvedValue("waiting");
    await finishWorktreeNaming("session", job({ retry }), Promise.resolve());
    clickRetry();
    clickRetry();
    await vi.waitFor(() => expect(retry).toHaveBeenCalledOnce());
    finish(null);
    await vi.waitFor(() => expect(toast.message).toHaveBeenCalledTimes(2));
    clickRetry();
    await vi.waitFor(() => expect(retry).toHaveBeenCalledTimes(2));
  });

  it("does not spend another model call after publication or an explicit branch change", async () => {
    const naming = job();
    vi.mocked(invoke)
      .mockResolvedValueOnce("waiting")
      .mockResolvedValueOnce("skipped");
    await finishWorktreeNaming("session", naming, Promise.resolve());
    clickRetry();
    await vi.waitFor(() =>
      expect(toast.message).toHaveBeenCalledWith(
        "Branch name kept",
        expect.anything(),
      ),
    );
    expect(naming.retry).not.toHaveBeenCalled();
    expect(invoke).not.toHaveBeenCalledWith("worktree_name", expect.anything());
    expect(toast.success).not.toHaveBeenCalled();
  });

  it("drops a retry result after the session is cancelled or removed", async () => {
    let current = true;
    let finish!: (branch: string | null) => void;
    const retry = vi.fn(
      () =>
        new Promise<string | null>((resolve) => {
          finish = resolve;
        }),
    );
    vi.mocked(invoke).mockResolvedValue("waiting");
    await finishWorktreeNaming(
      "session",
      job({ retry, isCurrent: () => current }),
      Promise.resolve(),
    );
    clickRetry();
    await vi.waitFor(() => expect(retry).toHaveBeenCalledOnce());
    current = false;
    finish("late-name");
    await vi.waitFor(() =>
      expect(invoke).toHaveBeenLastCalledWith("worktree_name", {
        ...request,
        branch: null,
      }),
    );
    expect(invoke).not.toHaveBeenCalledWith("worktree_name", {
      ...request,
      branch: "late-name",
    });
    expect(toast.success).not.toHaveBeenCalled();
  });

  it("retries a native apply error using the saved suggestion without another model call", async () => {
    const naming = job({ result: Promise.resolve("add-search") });
    vi.mocked(invoke)
      .mockRejectedValueOnce(new Error("Git rename failed"))
      .mockResolvedValueOnce("pending")
      .mockResolvedValueOnce("named");
    await finishWorktreeNaming("session", naming, Promise.resolve());
    expect(toast.message).toHaveBeenCalledWith(
      "Couldn't apply the AI branch name",
      expect.anything(),
    );
    clickRetry();
    await vi.waitFor(() => expect(toast.success).toHaveBeenCalled());
    expect(naming.retry).not.toHaveBeenCalled();
    expect(invoke).toHaveBeenLastCalledWith("worktree_name", {
      ...request,
      branch: "add-search",
    });
  });

  it("saves a retry result after failed setup without claiming the branch was renamed", async () => {
    vi.mocked(invoke)
      .mockResolvedValueOnce("waiting")
      .mockResolvedValueOnce("waiting")
      .mockResolvedValueOnce("pending");
    await finishWorktreeNaming(
      "session",
      job(),
      Promise.reject(new Error("Setup failed")),
    );
    clickRetry();
    await vi.waitFor(() =>
      expect(toast.message).toHaveBeenCalledWith(
        "AI name saved",
        expect.anything(),
      ),
    );
    expect(toast.success).not.toHaveBeenCalled();
  });

  it("does not show a naming warning for an existing or local checkout", async () => {
    vi.mocked(invoke).mockResolvedValue("skipped");
    await finishWorktreeNaming("session", job(), Promise.resolve());
    expect(toast.message).not.toHaveBeenCalled();
  });

  it("does not offer a futile retry when the provider has no naming capability", async () => {
    vi.mocked(invoke).mockResolvedValue("waiting");
    await finishWorktreeNaming(
      "session",
      job({ retry: undefined }),
      Promise.resolve(),
    );
    expect(toast.message).toHaveBeenCalledWith(
      "AI naming unavailable",
      expect.objectContaining({ action: undefined }),
    );
  });
});
