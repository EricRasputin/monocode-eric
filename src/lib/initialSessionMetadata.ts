import { generateHarnessTitle, type TitleInput } from "./harness/registry";
import type { HarnessId } from "./session";
import type { GeneratedSessionTitle } from "./sessionTitle";

/** Bound the whole metadata job, including time queued behind another request.
 * A late provider result is ignored; it never delays checkout or agent startup. */
export async function initialSessionMetadata(
  harness: HarnessId,
  input: TitleInput,
  timeoutMs = 45_000,
): Promise<GeneratedSessionTitle | null> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      generateHarnessTitle(harness, input).catch(() => null),
      new Promise<null>((resolve) => {
        timer = setTimeout(() => resolve(null), timeoutMs);
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

export function initialMessageContext(input: {
  message: string;
  plan?: string;
  handoff?: string;
  attachmentNames?: string[];
}): string {
  return [
    input.plan || input.message,
    input.handoff,
    input.attachmentNames?.length
      ? `Attachments: ${input.attachmentNames.join(", ")}`
      : undefined,
  ]
    .filter((part) => part?.trim())
    .join("\n\n");
}
