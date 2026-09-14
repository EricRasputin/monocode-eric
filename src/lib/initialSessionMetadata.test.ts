import { afterEach, describe, expect, it, vi } from "vitest";
import { generateHarnessTitle } from "./harness/registry";
import {
  initialMessageContext,
  initialSessionMetadata,
} from "./initialSessionMetadata";

vi.mock("./harness/registry", () => ({ generateHarnessTitle: vi.fn() }));
afterEach(() => {
  vi.useRealTimers();
  vi.resetAllMocks();
});

describe("first-message metadata", () => {
  const input = {
    sessionId: "session",
    cwd: "/repo",
    message: "Add search",
    includeBranch: true,
  };

  it("uses one provider request for title and branch", async () => {
    const result = {
      title: "Add search",
      branch: "add-search",
      workItem: null,
    };
    vi.mocked(generateHarnessTitle).mockResolvedValue(result);
    expect(await initialSessionMetadata("claude", input)).toEqual(result);
    expect(generateHarnessTitle).toHaveBeenCalledExactlyOnceWith(
      "claude",
      input,
    );
  });

  it("bounds queue and startup time and ignores late completion", async () => {
    vi.useFakeTimers();
    let finish!: (value: {
      title: string;
      branch: string;
      workItem: null;
    }) => void;
    vi.mocked(generateHarnessTitle).mockReturnValue(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    const result = initialSessionMetadata("codex", input, 50);
    await vi.advanceTimersByTimeAsync(50);
    expect(await result).toBeNull();
    finish({ title: "Late", branch: "too-late", workItem: null });
    expect(await result).toBeNull();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("treats missing providers and generation errors as optional metadata", async () => {
    vi.mocked(generateHarnessTitle)
      .mockResolvedValueOnce(null)
      .mockRejectedValueOnce(new Error("Offline"));
    expect(await initialSessionMetadata("fx", input)).toBeNull();
    expect(await initialSessionMetadata("codex", input)).toBeNull();
  });

  it("names the approved work instead of the generic build action", () => {
    expect(
      initialMessageContext({
        message: "Build approved plan",
        plan: "Add a searchable history panel",
        handoff: "Retain keyboard navigation",
        attachmentNames: ["history.png"],
      }),
    ).toBe(
      "Add a searchable history panel\n\nRetain keyboard navigation\n\nAttachments: history.png",
    );
    expect(
      initialMessageContext({
        message: "",
        attachmentNames: ["broken-menu.png"],
      }),
    ).toBe("Attachments: broken-menu.png");
  });
});
