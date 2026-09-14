import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";

export type WorktreeNaming = {
  token: string;
  result: Promise<string | null>;
  retry?: () => Promise<string | null>;
  isCurrent: () => boolean;
};

type NameStatus = "waiting" | "pending" | "named" | "skipped";

/** Finish naming outside the checkout/agent startup path. A failed model call
 * keeps the native request waiting so only an explicit retry can resume it.
 * Native ownership and publication checks still decide whether to rename. */
export async function finishWorktreeNaming(
  sessionId: string,
  naming: WorktreeNaming,
  setup: Promise<void>,
): Promise<void> {
  const id = `worktree-naming-${sessionId}`;
  const request = { sessionId, token: naming.token };
  let running = false;
  let finished = false;
  let suggestion: string | null = null;

  const readStatus = () => invoke<NameStatus>("worktree_name_status", request);

  const unavailable = (applying = false) => {
    toast.message(
      applying ? "Couldn't apply the AI branch name" : "AI naming unavailable",
      {
        id,
        duration: Infinity,
        closeButton: true,
        description: applying
          ? "Your worktree is still usable. Retry to check the naming operation."
          : "Kept the default branch name.",
        action:
          suggestion || naming.retry
            ? {
                label: "Retry",
                onClick: (event) => {
                  // Keep this toast mounted while it becomes the progress/result
                  // notice, instead of racing Sonner's action-dismiss animation.
                  event.preventDefault();
                  void retry();
                },
              }
            : undefined,
      },
    );
  };

  const showStatus = (status: NameStatus, manual: boolean) => {
    if (status === "waiting") {
      unavailable();
      return;
    }
    finished = true;
    if (!manual) {
      toast.dismiss(id);
      return;
    }
    if (status === "named") {
      toast.success("Worktree branch named", {
        id,
        action: undefined,
        description: undefined,
        duration: 6500,
      });
    } else if (status === "pending") {
      toast.message("AI name saved", {
        id,
        description: "It will be applied after worktree setup succeeds.",
        action: undefined,
        duration: 6500,
      });
    } else {
      toast.message("Branch name kept", {
        id,
        description:
          "This worktree is no longer eligible for automatic naming.",
        action: undefined,
        duration: 6500,
      });
    }
  };

  const cancelled = async () => {
    if (naming.isCurrent()) return false;
    finished = true;
    toast.dismiss(id);
    await invoke("worktree_name", { ...request, branch: null }).catch((error) =>
      console.debug("[monocode] cancel worktree name", error),
    );
    return true;
  };

  const apply = async (manual: boolean) => {
    if (await cancelled()) return;
    const status = suggestion
      ? await invoke<NameStatus>("worktree_name", {
          ...request,
          branch: suggestion,
        })
      : await readStatus();
    if (await cancelled()) return;
    showStatus(status, manual);
  };

  async function retry() {
    if (running || finished) return;
    running = true;
    try {
      if (await cancelled()) return;
      const status = await readStatus();
      if (status === "named" || status === "skipped") {
        showStatus(status, true);
        return;
      }
      toast.loading("Retrying AI worktree naming…", {
        id,
        action: undefined,
        description: undefined,
        duration: Infinity,
      });
      // Reuse a valid suggestion after a native error; only generation failure
      // needs another model call. Repeated clicks never queue duplicate calls.
      suggestion ||= (await naming.retry?.()) ?? null;
      await apply(true);
    } catch (error) {
      console.debug("[monocode] retry worktree name", error);
      if (naming.isCurrent()) unavailable(!!suggestion);
      else toast.dismiss(id);
    } finally {
      running = false;
    }
  }

  try {
    // A setup failure must remain visible to its caller. Save a valid name for
    // setup retry, while keeping naming failures out of that critical path.
    [suggestion] = await Promise.all([
      naming.result.catch(() => null),
      setup.catch(() => undefined),
    ]);
    await apply(false);
  } catch (error) {
    console.debug("[monocode] worktree name", error);
    if (naming.isCurrent()) unavailable(!!suggestion);
  }
}
