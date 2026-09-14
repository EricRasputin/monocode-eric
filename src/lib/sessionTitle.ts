import { extractJsonObject, limitSection } from "./jsonText";
import { sanitizeBranchFragment } from "./gitText";

const MESSAGE_LIMIT = 8_000;
const TITLE_LIMIT = 50;

const THREAD_TITLE_PROMPT = `Generate a title that will help the user recognize this coding session weeks later.
Also identify one GitHub issue or pull request only when the user explicitly refers to it by number or URL.
Return JSON with exactly two keys: title and workItem.
workItem must be null or an object with exactly two keys: kind ("issue" or "pr") and number (a positive integer copied from the user message).
Never invent a work item number. If the reference is ambiguous or has no number, return null.
Do not call tools. Reply with JSON only.

Before answering, silently reduce the request to:
- Subject: What system, feature, or problem is this really about?
- Outcome: What does the user ultimately want to understand or change?
- Incidental instructions: What only describes how the agent should do the work?

Title the subject and outcome. Discard incidental instructions.

Editorial rules:
- 3-8 words, fewer than 40 characters.
- Use a compact noun phrase or clear action phrase.
- Capture the umbrella goal when the request lists several symptoms or steps.
- Name the product change, not the mock, plan, report, branch, or PR used to produce it.
- Models, subagents, tools, and output formats do not belong in the title unless they are themselves the topic.
- Do not claim the work is complete.
- Do not copy and truncate the user's message.
- Avoid quotes, labels, filler, and trailing punctuation.`;

export type GeneratedWorkItemHint = {
  kind: "issue" | "pr";
  number: number;
};

export type GeneratedSessionTitle = {
  title: string;
  workItem: GeneratedWorkItemHint | null;
  branch?: string;
};

export function buildThreadTitlePrompt(
  message: string,
  includeBranch = false,
): string {
  const prompt = includeBranch
    ? THREAD_TITLE_PROMPT.replace(
        "Return JSON with exactly two keys: title and workItem.",
        "Return JSON with exactly three keys: title, workItem and branch.\n" +
          "branch is a concise Git branch fragment describing the same subject and outcome.\n" +
          "Use 2-6 lowercase words separated by hyphens, at most 64 characters.\n" +
          "Do not include a namespace, random suffix, issue prefix, or incidental instructions.",
      )
    : THREAD_TITLE_PROMPT;
  return `${prompt}\n\nUser message:\n${limitSection(message, MESSAGE_LIMIT)}`;
}

export function sanitizeThreadTitle(raw: string): string {
  const normalized = raw
    .trim()
    .split(/\r?\n/g)[0]
    ?.trim()
    .replace(/^['"`]+|['"`]+$/g, "")
    .trim()
    .replace(/\s+/g, " ");

  if (!normalized) return "";
  if (normalized.length <= TITLE_LIMIT) return normalized;
  return `${normalized.slice(0, TITLE_LIMIT - 3).trimEnd()}...`;
}

function referencedNumber(message: string, number: number): boolean {
  return new RegExp(`(^|\\D)${number}(?=\\D|$)`).test(message);
}

export function parseGeneratedSessionTitle(
  raw: string,
  message: string,
): GeneratedSessionTitle | null {
  const json = extractJsonObject(raw);
  if (json) {
    try {
      const parsed: unknown = JSON.parse(json);
      if (parsed && typeof parsed === "object") {
        const metadata = parsed as Record<string, unknown>;
        const title =
          typeof metadata.title === "string"
            ? sanitizeThreadTitle(metadata.title)
            : "";
        const branch =
          typeof metadata.branch === "string"
            ? sanitizeBranchFragment(metadata.branch)
            : "";
        const candidate = metadata.workItem;
        const workItem =
          candidate && typeof candidate === "object"
            ? (candidate as { kind?: unknown; number?: unknown })
            : null;
        const kind = workItem?.kind;
        const number = workItem?.number;
        const validWorkItem: GeneratedWorkItemHint | null =
          (kind === "issue" || kind === "pr") &&
          typeof number === "number" &&
          Number.isSafeInteger(number) &&
          number > 0 &&
          referencedNumber(message, number)
            ? { kind, number }
            : null;
        if (title || branch || validWorkItem) {
          return {
            title,
            workItem: validWorkItem,
            ...(branch ? { branch } : {}),
          };
        }
      }
    } catch {
      return null;
    }
  }
  // Providers can return quota/authentication failures as ordinary text.
  // Only the requested JSON fields are metadata; prose must never name a task.
  return null;
}

/** Backwards-compatible title-only parser for callers that do not need metadata. */
export function parseGeneratedThreadTitle(raw: string): string | null {
  return parseGeneratedSessionTitle(raw, "")?.title || null;
}
