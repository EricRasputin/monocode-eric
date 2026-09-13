import { invoke } from "@tauri-apps/api/core";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  clearInboxCache,
  githubPrDiff,
  githubWorkItemComment,
  githubWorkItemDetails,
  githubWorkItemThread,
  listInboxItems,
  peekGithubPrDiff,
  peekGithubWorkItemDetails,
  peekGithubWorkItemThread,
  type GithubTaskKind,
} from "./githubTasks";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const query = { assignedToMe: false, state: "open" as const, search: "" };
const reposByPath: Record<string, string[]> = {
  "/fork": ["me/widget", "acme/widget"],
  "/other-fork": ["other/widget", "Acme/Widget"],
  "/upstream": ["acme/widget"],
  "/plain": ["team/docs"],
};

type Request = {
  cwd: string;
  repo: string;
  kind: GithubTaskKind;
  number: number;
};

beforeEach(() => {
  clearInboxCache();
  vi.mocked(invoke).mockReset();
  vi.mocked(invoke).mockImplementation(async (command, args) => {
    const { cwd, repo, kind, number } = (args ?? {}) as Request;
    switch (command) {
      case "linear_status":
      case "gitlab_status":
        return { connected: false };
      case "git_github_inbox_repos":
        if (!reposByPath[cwd]) throw new Error("Repository unavailable");
        return reposByPath[cwd];
      case "git_github_work_items":
        return [
          {
            kind,
            repo,
            number: 10,
            title: repo,
            state: "open",
            draft: false,
            url: `https://github.com/${repo}/${kind === "pr" ? "pull" : "issues"}/10`,
            updatedAt: "2026-09-14T00:00:00Z",
            labels: [],
            assignees: [],
          },
        ];
      case "git_github_work_item_details":
        return { body: repo, author: "author" };
      case "git_github_work_item_thread":
        return {
          comments: [],
          commits: [],
          truncated: false,
          reviewDecision: "",
          baseRefName: repo,
          headRefName: "feature",
        };
      case "git_github_pr_diff":
        return {
          additions: 1,
          deletions: 0,
          files: [],
          patch: repo,
          truncated: false,
        };
      case "git_github_work_item_comment":
        return `https://github.com/${repo}/${kind === "pr" ? "pull" : "issues"}/${number}#comment`;
      default:
        throw new Error(`Unexpected command: ${command}`);
    }
  });
});

describe("fork repositories in Inbox", () => {
  it("lists both repositories and keeps the local fork as the work destination", async () => {
    const result = await listInboxItems([{ path: "/fork" }], query);
    expect(result.errors).toEqual({});
    expect(result.items).toHaveLength(4);
    expect(
      result.items.map(({ repo, kind, projectPath }) => ({
        repo,
        kind,
        projectPath,
      })),
    ).toEqual(
      expect.arrayContaining([
        { repo: "me/widget", kind: "issue", projectPath: "/fork" },
        { repo: "me/widget", kind: "pr", projectPath: "/fork" },
        { repo: "acme/widget", kind: "issue", projectPath: "/fork" },
        { repo: "acme/widget", kind: "pr", projectPath: "/fork" },
      ]),
    );
    expect(invoke).toHaveBeenCalledWith("git_github_work_items", {
      cwd: "/fork",
      repo: "acme/widget",
      kind: "issue",
      ...query,
      limit: undefined,
    });
  });

  it("fetches a shared upstream once even when it is also a separate project", async () => {
    const result = await listInboxItems(
      ["/fork", "/other-fork", "/upstream"].map((path) => ({ path })),
      query,
    );
    const upstream = result.items.filter(
      (item) => item.repo.toLowerCase() === "acme/widget",
    );
    expect(upstream).toHaveLength(2);
    expect(upstream.every((item) => item.projectPath === "/fork")).toBe(true);
    const listCalls = vi
      .mocked(invoke)
      .mock.calls.filter(([command]) => command === "git_github_work_items");
    expect(listCalls).toHaveLength(6);
  });

  it("preserves filtering and ordinary repositories", async () => {
    const result = await listInboxItems([{ path: "/plain" }], {
      assignedToMe: true,
      state: "all",
      search: "  fix  ",
    });
    expect(result.items).toHaveLength(2);
    expect(result.items.every((item) => item.repo === "team/docs")).toBe(true);
    expect(invoke).toHaveBeenCalledWith("git_github_work_items", {
      cwd: "/plain",
      repo: "team/docs",
      kind: "pr",
      assignedToMe: true,
      state: "all",
      search: "fix",
      limit: 100,
    });
  });

  it("keeps upstream items if the fork has issues disabled", async () => {
    const implementation = vi.mocked(invoke).getMockImplementation()!;
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      const request = args as Request;
      if (
        command === "git_github_work_items" &&
        request.repo === "me/widget" &&
        request.kind === "issue"
      ) {
        throw new Error("Issues are disabled for this repo");
      }
      return implementation(command, args);
    });
    const result = await listInboxItems([{ path: "/fork" }], query);
    expect(result.items).toHaveLength(3);
    expect(
      result.items.filter((item) => item.repo === "acme/widget"),
    ).toHaveLength(2);
  });

  it("keeps successful projects and reports discovery errors when none succeed", async () => {
    const partial = await listInboxItems(
      [{ path: "/unavailable" }, { path: "/plain" }],
      query,
    );
    expect(partial.items).toHaveLength(2);
    expect(partial.errors).toEqual({});
    const failed = await listInboxItems([{ path: "/unavailable" }], query);
    expect(failed).toEqual({
      items: [],
      errors: { github: "Repository unavailable" },
    });
  });
});

describe("repository identity for Inbox operations", () => {
  it("keeps details for the same number in fork and upstream separate", async () => {
    await Promise.all([
      githubWorkItemDetails("/fork", "me/widget", "issue", 10),
      githubWorkItemDetails("/fork", "acme/widget", "issue", 10),
    ]);
    expect(peekGithubWorkItemDetails("me/widget", "issue", 10)?.body).toBe(
      "me/widget",
    );
    expect(peekGithubWorkItemDetails("ACME/Widget", "issue", 10)?.body).toBe(
      "acme/widget",
    );
    expect(invoke).toHaveBeenCalledWith("git_github_work_item_details", {
      cwd: "/fork",
      repo: "acme/widget",
      kind: "issue",
      number: 10,
    });
  });

  it("separates concurrent threads and diffs by repository, sharing across checkouts", async () => {
    await Promise.all([
      githubWorkItemThread("/fork", "me/widget", "pr", 10),
      githubWorkItemThread("/fork", "acme/widget", "pr", 10),
      githubWorkItemThread("/upstream", "ACME/Widget", "pr", 10),
      githubPrDiff("/fork", "me/widget", 10),
      githubPrDiff("/fork", "acme/widget", 10),
      githubPrDiff("/upstream", "ACME/Widget", 10),
    ]);
    expect(invoke).toHaveBeenCalledTimes(4);
    expect(peekGithubWorkItemThread("me/widget", "pr", 10)?.baseRefName).toBe(
      "me/widget",
    );
    expect(peekGithubWorkItemThread("acme/widget", "pr", 10)?.baseRefName).toBe(
      "acme/widget",
    );
    expect(peekGithubPrDiff("me/widget", 10)?.patch).toBe("me/widget");
    expect(peekGithubPrDiff("acme/widget", 10)?.patch).toBe("acme/widget");
    expect(invoke).toHaveBeenCalledWith("git_github_work_item_thread", {
      cwd: "/fork",
      repo: "acme/widget",
      kind: "pr",
      number: 10,
    });
    expect(invoke).toHaveBeenCalledWith("git_github_pr_diff", {
      cwd: "/fork",
      repo: "acme/widget",
      number: 10,
    });
  });

  it.each(["issue", "pr"] as const)(
    "targets %s comments and invalidates only that repository's thread",
    async (kind) => {
      await Promise.all([
        githubWorkItemThread("/fork", "me/widget", kind, 10),
        githubWorkItemThread("/fork", "acme/widget", kind, 10),
      ]);
      const inReplyTo = kind === "pr" ? "PRRT_thread" : "";
      await githubWorkItemComment("/fork", "acme/widget", kind, 10, " hello ", {
        inReplyTo,
      });
      expect(invoke).toHaveBeenCalledWith("git_github_work_item_comment", {
        cwd: "/fork",
        repo: "acme/widget",
        kind,
        number: 10,
        body: "hello",
        inReplyTo,
      });
      expect(peekGithubWorkItemThread("acme/widget", kind, 10)).toBeNull();
      expect(peekGithubWorkItemThread("me/widget", kind, 10)).not.toBeNull();
    },
  );
});
