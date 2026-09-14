import { describe, expect, it } from "vitest";
import {
  buildThreadTitlePrompt,
  parseGeneratedSessionTitle,
} from "./sessionTitle";

describe("session title metadata", () => {
  it("requests a semantic branch only for new worktrees", () => {
    expect(buildThreadTitlePrompt("Add search", true)).toContain(
      "title, workItem and branch",
    );
    expect(buildThreadTitlePrompt("Add search", true)).toContain(
      "2-6 lowercase words",
    );
    expect(buildThreadTitlePrompt("Add search")).not.toContain(
      "workItem and branch",
    );
  });

  it("parses title, branch and work item independently", () => {
    expect(
      parseGeneratedSessionTitle(
        '{"title":"Add search","branch":"Add Search!","workItem":null}',
        "Add search",
      ),
    ).toEqual({ title: "Add search", branch: "add-search", workItem: null });
    expect(
      parseGeneratedSessionTitle(
        '{"title":"Add search","branch":{},"workItem":null}',
        "Add search",
      ),
    ).toEqual({ title: "Add search", workItem: null });
    expect(
      parseGeneratedSessionTitle(
        '{"title":null,"branch":"add-search","workItem":null}',
        "Add search",
      ),
    ).toEqual({ title: "", branch: "add-search", workItem: null });
    expect(
      parseGeneratedSessionTitle(
        '{"title":null,"branch":[],"workItem":null}',
        "Add search",
      ),
    ).toBeNull();
  });

  it("asks the title pass for one optional work item", () => {
    expect(buildThreadTitlePrompt("Fix PR #42")).toContain(
      "title and workItem",
    );
  });

  it("accepts a referenced PR number", () => {
    expect(
      parseGeneratedSessionTitle(
        '{"title":"Fix session links","workItem":{"kind":"pr","number":42}}',
        "Please fix PR #42",
      ),
    ).toEqual({
      title: "Fix session links",
      workItem: { kind: "pr", number: 42 },
    });
  });

  it("drops a model-invented number without losing the title", () => {
    expect(
      parseGeneratedSessionTitle(
        '{"title":"Fix session links","workItem":{"kind":"issue","number":99}}',
        "Please fix the session links",
      ),
    ).toEqual({ title: "Fix session links", workItem: null });
  });

  it.each([
    "Upgrade your plan to continue",
    "You have reached your usage limit",
    "Rate limit exceeded",
    "Please sign in to continue",
    "Fix session links",
    '{"error":"Upgrade your plan to continue"}',
    '{"title":"Broken JSON"',
  ])("rejects unstructured or invalid metadata: %s", (output) => {
    expect(parseGeneratedSessionTitle(output, "Add search")).toBeNull();
  });

  it("accepts structured metadata wrapped in a code fence", () => {
    expect(
      parseGeneratedSessionTitle(
        '```json\n{"title":"Add search","branch":"add-search","workItem":null}\n```',
        "Add search",
      ),
    ).toEqual({ title: "Add search", branch: "add-search", workItem: null });
  });
});
