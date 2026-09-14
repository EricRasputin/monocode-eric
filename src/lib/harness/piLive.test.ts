import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  close: vi.fn(),
  request: vi.fn(),
  resolveBinary: vi.fn(),
  spawnChild: vi.fn(),
  killChild: vi.fn(),
  frames: [] as Array<(record: Record<string, unknown>) => void>,
}));

vi.mock("./child", () => ({
  killChild: mocks.killChild,
  resolveOmpBinary: vi.fn(),
  resolvePiBinary: mocks.resolveBinary,
  spawnChild: mocks.spawnChild,
  unwatchChild: vi.fn(),
  watchChild: vi.fn(),
  writeChild: vi.fn(),
}));

vi.mock("./piClient", () => ({
  PiRpc: class {
    constructor(
      _sessionId: string,
      onFrame: (record: Record<string, unknown>) => void,
    ) {
      mocks.frames.push(onFrame);
    }

    request = mocks.request;
    close = mocks.close;
    cancelRequest = vi.fn();
    pushLine = vi.fn();
  },
}));

import { compactPiContext, sendPiTurn, stopPiSession } from "./pi";
import type { HarnessEvent } from "./types";

describe("Pi live session", () => {
  beforeEach(() => {
    mocks.close.mockReset();
    mocks.request.mockReset();
    mocks.resolveBinary.mockReset();
    mocks.spawnChild.mockReset();
    mocks.killChild.mockReset();
    mocks.frames.length = 0;
    mocks.resolveBinary.mockResolvedValue({ path: "/fake/pi" });
    mocks.spawnChild.mockResolvedValue(undefined);
    mocks.killChild.mockResolvedValue(undefined);
    mocks.request.mockImplementation(
      async (command: Record<string, unknown>) => {
        if (command.type === "get_state") {
          return {
            data: {
              sessionId: "pi_session",
              model: { contextWindow: 200_000 },
            },
          };
        }
        if (command.type === "compact") {
          return { data: { estimatedTokensAfter: 32_000 } };
        }
        return { data: {} };
      },
    );
  });

  it("uses the compact RPC command and publishes the post-compact estimate", async () => {
    const events: HarnessEvent[] = [];

    await compactPiContext({
      sessionId: "pi-compact",
      cwd: "/repo",
      model: "pi:default",
      runtimeMode: "supervised",
      onEvent: (event) => events.push(event),
    });

    expect(mocks.request).toHaveBeenCalledWith(
      { type: "compact" },
      30 * 60_000,
    );
    expect(events).toContainEqual({
      type: "context",
      used: 32_000,
      window: 200_000,
    });
    await stopPiSession("pi-compact");
  });

  it("totals assistant requests once across streaming and terminal usage snapshots", async () => {
    const events: HarnessEvent[] = [];
    const input = {
      sessionId: "pi-metrics",
      cwd: "/repo",
      model: "pi:default",
      modelSettings: {},
      runtimeMode: "supervised" as const,
      text: "Check and summarize",
      attachments: [],
      onEvent: (event: HarnessEvent) => events.push(event),
    };
    const turn = sendPiTurn(input);
    await vi.waitFor(() =>
      expect(mocks.request).toHaveBeenCalledWith(
        expect.objectContaining({ type: "prompt" }),
        expect.any(Number),
      ),
    );
    const frame = mocks.frames[0]!;
    const first = {
      role: "assistant",
      usage: { input: 100, output: 20, cacheRead: 40 },
    };
    frame({ type: "message_start", message: { role: "assistant" } });
    frame({ type: "message_update", usage: first.usage });
    frame({ type: "message_end", message: first });
    frame({ type: "turn_end", message: first });
    frame({ type: "message_start", message: { role: "toolResult" } });
    frame({ type: "message_start", message: { role: "assistant" } });
    const second = {
      role: "assistant",
      usage: { input: 200, output: 30, cacheRead: 100 },
    };
    frame({
      type: "message_update",
      assistantMessageEvent: { partial: second },
    });
    frame({ type: "message_end", message: second });
    frame({ type: "turn_end", message: second });
    frame({ type: "agent_end", messages: [first, second] });
    await turn;
    expect(
      events.filter((event) => event.type === "turn.metrics").at(-1),
    ).toMatchObject({
      inputTokens: 300,
      outputTokens: 50,
      cacheReadTokens: 140,
      cacheHitPercent: (140 / 440) * 100,
    });
    expect(
      events.filter((event) => event.type === "context").at(-1),
    ).toMatchObject({ used: 330, window: 200_000 });
    const nextTurn = sendPiTurn({ ...input, text: "Follow up" });
    await vi.waitFor(() =>
      expect(
        mocks.request.mock.calls.filter(
          ([command]) => command.type === "prompt",
        ),
      ).toHaveLength(2),
    );
    const followup = {
      role: "assistant",
      usage: { input: 50, output: 10, cacheRead: 0 },
    };
    frame({ type: "message_start", message: { role: "assistant" } });
    frame({ type: "message_end", message: followup });
    frame({ type: "turn_end", message: followup });
    frame({ type: "agent_end", messages: [followup] });
    await nextTurn;
    expect(
      events.filter((event) => event.type === "turn.metrics").at(-1),
    ).toMatchObject({
      inputTokens: 50,
      outputTokens: 10,
      cacheReadTokens: 0,
      cacheHitPercent: 0,
    });
    await stopPiSession("pi-metrics");
  });

  it("publishes readable Ponytail status and extension notifications", async () => {
    const events: HarnessEvent[] = [];
    await compactPiContext({
      sessionId: "pi-ansi",
      cwd: "/repo",
      model: "pi:default",
      runtimeMode: "supervised",
      onEvent: (event) => events.push(event),
    });
    const frame = mocks.frames[0]!;
    frame({
      type: "extension_ui_request",
      id: "ponytail-status",
      method: "setStatus",
      statusKey: "ponytail",
      statusText:
        "\u001b[38;5;241m○\u001b[39m \u001b[38;5;244mponytail:\u001b[39m \u001b[38;5;188m⚡ FULL\u001b[0m",
    });
    frame({
      type: "extension_ui_request",
      id: "plugin-notify",
      method: "notify",
      message: "\u001b[32mPlugin ready\u001b[0m",
    });
    frame({
      type: "extension_ui_request",
      id: "empty-status",
      method: "setStatus",
      statusText: "\u001b[0m",
    });
    expect(events.filter((event) => event.type === "status")).toEqual([
      { type: "status", text: "○ ponytail: ⚡ FULL" },
      { type: "status", text: "Plugin ready" },
    ]);
    await stopPiSession("pi-ansi");
  });
});
