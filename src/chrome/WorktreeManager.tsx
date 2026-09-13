import { useState } from "react";
import { prettyCwd, projectName } from "../lib/paths";
import { GitBranch } from "./icons";
import {
  cleanupWorktrees,
  pinWorktree,
  suggestedWorktrees,
  type WorktreeEntry,
} from "../lib/worktrees";
import { refreshWorktrees, useWorktrees } from "../hooks/useWorktrees";

const button =
  "rounded-md border border-content/15 px-2.5 py-1.5 text-[12px] hover:bg-content/8 disabled:opacity-40";

/** Creation lives in the composer. This screen is only for reviewing disk cleanup. */
export function WorktreeManager({
  cwd,
  onOpen,
}: {
  cwd: string;
  onOpen: (entry: WorktreeEntry) => Promise<void>;
}) {
  const { overview, error: loadError, pending } = useWorktrees(cwd);
  const [selection, setSelection] = useState<string[] | null>(null);
  const [reviewIds, setReviewIds] = useState<string[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const entries = overview?.entries ?? [];
  const ready = entries.filter(
    (entry) => entry.id && !entry.blockedReason && !entry.missing,
  );
  const suggested = overview ? suggestedWorktrees(overview) : [];
  const selected = (selection ?? suggested.map((entry) => entry.id!)).filter(
    (id) => ready.some((entry) => entry.id === id),
  );
  const matches = (entry: WorktreeEntry) =>
    `${entry.branch ?? ""} ${entry.path}`
      .toLowerCase()
      .includes(query.toLowerCase());
  const kept = entries.filter((entry) => !ready.includes(entry));
  async function run(operation: () => Promise<void>) {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      await operation();
      await refreshWorktrees(cwd);
    } catch (error) {
      setError(String(error));
    } finally {
      setBusy(false);
    }
  }
  const displayName = (entry: WorktreeEntry) => entry.branch ?? "Detached HEAD";
  if (!cwd || cwd === "~") {
    return (
      <p className="text-sm text-content/60">
        Open a Git project to review its worktrees.
      </p>
    );
  }
  return (
    <section aria-label="Worktree cleanup" className="space-y-4">
      <div className="rounded-lg border border-content/10 px-4 py-3">
        <h2 className="text-sm font-medium">{projectName(cwd)}</h2>
        <p className="mt-1 break-all font-mono text-[11px] text-content/45">
          {prettyCwd(cwd)}
        </p>
      </div>
      <p className="text-[12px] leading-relaxed text-content/60">
        Monocode suggests unused worktrees after 7 days. You decide what to
        remove.
      </p>
      {pending && (
        <p role="status" className="text-[12px] text-content/60">
          Checking worktrees…
        </p>
      )}
      <input
        aria-label="Find worktree to clean up"
        placeholder="Find a worktree…"
        value={query}
        onChange={(event) => setQuery(event.target.value)}
        className="w-full rounded-md border border-content/15 bg-content/5 px-3 py-2 text-[12px] outline-none focus:border-accent"
      />
      <div className="flex items-center justify-between text-[12px]">
        <span className="font-medium">Ready to remove ({ready.length})</span>
        <button
          className={button}
          disabled={busy}
          onClick={() => void run(() => refreshWorktrees(cwd))}
        >
          Refresh
        </button>
      </div>
      <div className="space-y-2">
        {ready.filter(matches).map((entry) => (
          <div
            key={entry.id}
            className="rounded-lg border border-content/10 p-3"
          >
            <label className="flex cursor-pointer items-center gap-2 text-[12px]">
              <input
                type="checkbox"
                aria-label={`Select ${displayName(entry)}`}
                checked={selected.includes(entry.id!)}
                disabled={busy || !!reviewIds}
                onChange={(event) =>
                  setSelection(
                    event.target.checked
                      ? [...selected, entry.id!]
                      : selected.filter((id) => id !== entry.id),
                  )
                }
              />
              <GitBranch className="size-3.5 shrink-0" />
              <span className="min-w-0 flex-1 truncate font-mono">
                {displayName(entry)}
              </span>
              {suggested.some((item) => item.id === entry.id) && (
                <span className="text-[10px] text-content/50">Suggested</span>
              )}
            </label>
            <div className="mt-2 flex items-center gap-2 pl-6">
              <p
                className="min-w-0 flex-1 truncate text-[11px] text-content/45"
                title={entry.path}
              >
                {entry.path}
              </p>
              <button
                className={button}
                disabled={busy || !!reviewIds}
                onClick={() => void run(() => pinWorktree(entry.id!, true))}
              >
                Pin
              </button>
            </div>
          </div>
        ))}
        {overview && !ready.length && (
          <p className="rounded-lg bg-content/3 p-3 text-[12px] text-content/55">
            Nothing to clean up. Archive finished conversations to make their
            worktrees eligible.
          </p>
        )}
      </div>
      {reviewIds ? (
        <div className="space-y-3 rounded-lg border border-content/20 p-3 text-[12px]">
          <p>
            Remove these {reviewIds.length} checkout
            {reviewIds.length === 1 ? "" : "s"}? Branches and saved
            conversations will be kept.
          </p>
          <ul className="max-h-24 overflow-y-auto font-mono text-[11px] text-content/60">
            {reviewIds.map((id) => (
              <li key={id}>
                {entries.find((entry) => entry.id === id)?.branch ?? id}
              </li>
            ))}
          </ul>
          <div className="flex gap-2">
            <button
              className={button}
              disabled={busy}
              onClick={() =>
                void run(async () => {
                  const report = await cleanupWorktrees(cwd, reviewIds);
                  setReviewIds(null);
                  setSelection([]);
                  setNotice(
                    `Removed ${report.removed.length} checkout(s). Branches kept.${report.skipped.length ? ` Kept: ${report.skipped.join("; ")}` : ""}`,
                  );
                })
              }
            >
              Remove checkouts
            </button>
            <button
              className={button}
              disabled={busy}
              onClick={() => setReviewIds(null)}
            >
              Cancel
            </button>
          </div>
        </div>
      ) : (
        <button
          className={`${button} w-full bg-content/5`}
          disabled={busy || !selected.length}
          onClick={() => setReviewIds([...selected])}
        >
          Review removal ({selected.length})
        </button>
      )}
      <details className="border-t border-content/10 pt-3">
        <summary className="cursor-pointer text-[12px] text-content/60">
          Kept worktrees ({kept.length})
        </summary>
        <div className="mt-3 space-y-2">
          {kept.filter(matches).map((entry) => (
            <div
              key={entry.path}
              className="rounded-lg border border-content/10 p-3 text-[12px]"
            >
              <div className="flex items-center gap-2">
                <GitBranch className="size-3.5 shrink-0" />
                <span
                  className="min-w-0 flex-1 truncate font-mono"
                  title={entry.path}
                >
                  {displayName(entry)}
                </span>
                {entry.id && (
                  <button
                    className={button}
                    disabled={busy}
                    onClick={() =>
                      void run(() => pinWorktree(entry.id!, !entry.pinned))
                    }
                  >
                    {entry.pinned ? "Unpin" : "Pin"}
                  </button>
                )}
                <button
                  className={button}
                  disabled={busy}
                  onClick={() => void run(() => onOpen(entry))}
                >
                  {entry.missing ? "Restore" : "Open"}
                </button>
              </div>
              <p className="mt-2 text-[11px] text-content/50">
                {entry.blockedReason}
              </p>
            </div>
          ))}
        </div>
      </details>
      {busy && (
        <p role="status" className="text-[12px] text-content/60">
          Checking…
        </p>
      )}
      {notice && (
        <p role="status" className="text-[12px] text-content/70">
          {notice}
        </p>
      )}
      {(error || loadError) && (
        <p role="alert" className="text-[12px] text-red-400">
          {error || loadError}
        </p>
      )}
    </section>
  );
}
