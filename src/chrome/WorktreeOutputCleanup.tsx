import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import type { WorktreeEntry } from "../lib/worktrees";
import {
  executeWorktreeOutputCleanup,
  getWorktreeOutputHistory,
  reviewWorktreeOutputs,
  type OutputCleanupReport,
  type OutputReview,
} from "../lib/worktreeOutputCleanup";
import { diskBytes as formatDiskBytes } from "../lib/worktreeDisk";

const button =
  "rounded-md border border-content/10 px-2.5 py-1.5 text-[12px] text-content/70 hover:bg-content/8 disabled:opacity-40";
const message = (error: unknown) =>
  error instanceof Error ? error.message : String(error);

/** Manual review is independent of retirement eligibility: unfinished source
 * and an idle unarchived conversation may keep their checkout and clear outputs. */
export function WorktreeOutputCleanup({
  cwd,
  entries,
  disabled = false,
  onChanged,
  onReviewingChange,
}: {
  cwd: string;
  entries: WorktreeEntry[];
  disabled?: boolean;
  onChanged: () => Promise<void> | void;
  onReviewingChange?: (reviewing: boolean) => void;
}) {
  const checkouts = entries.filter(
    (entry) => entry.id && !entry.main && !entry.missing,
  );
  const [chosen, setChosen] = useState("");
  const id = checkouts.some((entry) => entry.id === chosen)
    ? chosen
    : (checkouts[0]?.id ?? "");
  const [review, setReview] = useState<OutputReview | null>(null);
  const [selection, setSelection] = useState<string[]>([]);
  const [reports, setReports] = useState<OutputCleanupReport[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const historySequence = useRef(0);
  const [historyError, setHistoryError] = useState<string | null>(null);
  useEffect(() => {
    onReviewingChange?.(busy || !!review);
    return () => onReviewingChange?.(false);
  }, [busy, review, onReviewingChange]);
  useEffect(() => {
    let active = true;
    const stops: (() => void)[] = [];
    const refresh = () => {
      const sequence = ++historySequence.current;
      void getWorktreeOutputHistory(cwd)
        .then((rows) => {
          if (active && sequence === historySequence.current) {
            setReports(rows);
            setHistoryError(null);
          }
        })
        .catch((cause) => {
          if (active && sequence === historySequence.current)
            setHistoryError(message(cause));
        });
    };
    refresh();
    for (const event of [
      "worktree-output-changed",
      "worktree-setup-progress",
    ]) {
      void listen(event, refresh)
        .then((stop) => {
          if (active) stops.push(stop);
          else stop();
        })
        .catch(() => undefined);
    }
    return () => {
      active = false;
      for (const stop of stops) stop();
    };
  }, [cwd]);

  async function inspect() {
    if (!id) return;
    setBusy(true);
    setError(null);
    try {
      const next = await reviewWorktreeOutputs(cwd, id);
      setReview(next);
      setSelection(
        next.blockedReason
          ? []
          : next.candidates.filter((c) => !c.blockedReason).map((c) => c.path),
      );
    } catch (cause) {
      setError(message(cause));
    } finally {
      setBusy(false);
    }
  }
  async function execute() {
    if (!review || !selection.length) return;
    setBusy(true);
    setError(null);
    try {
      const report = await executeWorktreeOutputCleanup(
        review.planId,
        selection,
      );
      historySequence.current += 1;
      setReports((rows) => [
        report,
        ...rows.filter((row) => row.planId !== report.planId),
      ]);
      setReview(null);
      setSelection([]);
    } catch (cause) {
      setError(`${message(cause)}. Review again before retrying.`);
      // A lost response may follow partial deletion. The durable journal is
      // authoritative, and this review must never offer a blind execute retry.
      setReview(null);
      try {
        setReports(await getWorktreeOutputHistory(cwd));
      } catch (cause) {
        setHistoryError(message(cause));
      }
    } finally {
      try {
        await onChanged();
      } catch (cause) {
        setHistoryError(`Could not refresh usage: ${message(cause)}`);
      }
      setBusy(false);
    }
  }
  const eligible =
    review?.candidates.filter(
      (candidate) =>
        !candidate.blockedReason && selection.includes(candidate.path),
    ) ?? [];
  return (
    <section
      aria-label="Clear generated files"
      className="space-y-3 rounded-lg border border-content/10 p-4"
    >
      <div>
        <h2 className="text-[13px] font-medium">Clear generated files</h2>
        <p className="mt-1 text-[12px] leading-relaxed text-content/50">
          Free space in an idle worktree while keeping its checkout, unfinished
          source, branches and conversations. This action is manual.
        </p>
      </div>
      <p className="text-[12px] leading-relaxed text-content/50">
        Only recognized or configured disposable folders are offered. Selected
        local configuration stays in place, including files inside output
        folders. Live workspaces, pins, agents, terminals, setup and Git locks
        block cleanup. An idle unarchived conversation alone does not.
      </p>
      {!checkouts.length ? (
        <p className="text-[12px] text-content/50">
          No managed checkouts available for output review.
        </p>
      ) : (
        <div className="flex flex-wrap items-end gap-2">
          <label className="min-w-0 flex-1 text-[12px] text-content/70">
            Worktree to clear
            <select
              aria-label="Worktree to clear"
              value={id}
              disabled={disabled || busy || !!review}
              onChange={(event) => {
                setChosen(event.target.value);
                setError(null);
              }}
              className="mt-1.5 block w-full rounded-md border border-content/10 bg-surface px-2.5 py-2 text-[12px]"
            >
              {checkouts.map((entry) => (
                <option key={entry.id} value={entry.id!}>
                  {entry.branch ?? entry.path}
                </option>
              ))}
            </select>
          </label>
          <button
            className={button}
            disabled={disabled || busy || !!review || !id}
            onClick={() => void inspect()}
          >
            Review generated files
          </button>
        </div>
      )}
      {review ? (
        <div
          aria-label="Output cleanup review"
          className="space-y-3 border-t border-content/10 pt-3"
        >
          <p className="break-all text-[12px] font-medium">{review.path}</p>
          {review.blockedReason ? (
            <p role="alert" className="text-[12px] text-amber-500">
              {review.blockedReason}
            </p>
          ) : null}
          <ul className="divide-y divide-content/5">
            {review.candidates.map((candidate) => (
              <li key={candidate.path} className="py-2">
                <label className="flex items-start gap-2 text-[12px]">
                  <input
                    type="checkbox"
                    aria-label={`Clear ${candidate.path}`}
                    checked={selection.includes(candidate.path)}
                    disabled={
                      disabled ||
                      busy ||
                      !!review.blockedReason ||
                      !!candidate.blockedReason
                    }
                    onChange={(event) =>
                      setSelection((current) =>
                        event.target.checked
                          ? [...current, candidate.path]
                          : current.filter((path) => path !== candidate.path),
                      )
                    }
                  />
                  <span className="min-w-0 break-all">
                    {candidate.path}{" "}
                    <span className="text-content/45">
                      · {formatDiskBytes(candidate.estimatedBytes)} estimated
                    </span>
                  </span>
                </label>
                {candidate.blockedReason ? (
                  <p className="mt-1 text-[12px] text-amber-500">
                    Kept: {candidate.blockedReason}
                  </p>
                ) : null}
                {candidate.preservedPaths.map((path) => (
                  <p
                    key={path}
                    className="mt-1 break-all text-[12px] text-content/60"
                  >
                    Preserved in place: {path}
                  </p>
                ))}
              </li>
            ))}
          </ul>
          {!review.candidates.length ? (
            <p className="text-[12px] text-content/55">
              No recognized or configured output directories found.
            </p>
          ) : null}
          <p className="text-[12px] text-content/60">
            Setup will run before the next coding, file or terminal action.
            Failed setup can be retried; unfinished source stays in place.
          </p>
          <p className="text-[11px] text-content/45">
            {formatDiskBytes(
              eligible.reduce(
                (sum, candidate) => sum + candidate.estimatedBytes,
                0,
              ),
            )}{" "}
            estimated for selected files. Shared storage and other disk activity
            affect actual free space.
          </p>
          <div className="flex flex-wrap gap-2">
            <button
              className={button}
              disabled={
                disabled || busy || !!review.blockedReason || !eligible.length
              }
              onClick={() => void execute()}
            >
              Clear selected outputs ({eligible.length})
            </button>
            <button
              className={button}
              disabled={busy}
              onClick={() => {
                setReview(null);
                setSelection([]);
              }}
            >
              Cancel review
            </button>
          </div>
        </div>
      ) : null}
      {busy ? (
        <p role="status" className="text-[12px] text-content/55">
          Checking generated files…
        </p>
      ) : null}
      {error ? (
        <p role="alert" className="text-[12px] text-red-400">
          {error}
        </p>
      ) : null}
      {historyError ? (
        <p role="alert" className="text-[12px] text-amber-500">
          Cleanup history or usage refresh failed: {historyError}
        </p>
      ) : null}
      {reports.length ? (
        <details open className="border-t border-content/10 pt-3">
          <summary className="cursor-pointer text-[12px] font-medium">
            Recent output cleanup
          </summary>
          <div className="mt-2 space-y-4">
            {reports.slice(0, 5).map((report) => (
              <CleanupResult key={report.planId} report={report} />
            ))}
          </div>
        </details>
      ) : null}
    </section>
  );
}

function CleanupResult({ report }: { report: OutputCleanupReport }) {
  const interrupted =
    report.status === "interrupted" || report.status === "executing";
  const labels = {
    complete: "Outputs cleared",
    partial: "Some outputs were kept",
    interrupted: "Cleanup was interrupted",
    executing: "Cleanup in progress",
  };
  const change = report.observedFreeSpaceChange;
  return (
    <div className="space-y-1 text-[12px]" aria-label="Output cleanup result">
      <p className="font-medium">{labels[report.status]}</p>
      <p className="break-all text-content/55">{report.path}</p>
      <p className="text-content/70">
        Estimated removed bytes: {formatDiskBytes(report.estimatedRemovedBytes)}
        {interrupted ? " (saved results only)" : ""}
      </p>
      <p className="text-content/60">
        Observed filesystem free-space change:{" "}
        {change == null
          ? "unavailable"
          : `${change > 0 ? "+" : change < 0 ? "−" : ""}${formatDiskBytes(Math.abs(change))}`}
      </p>
      <p className="text-[11px] text-content/45">
        Free-space change includes other filesystem activity and may differ from
        removed-file estimates.
      </p>
      {report.measurementError ? (
        <p className="text-amber-500">
          Measurement unavailable: {report.measurementError}
        </p>
      ) : null}
      {report.results.map((result) => (
        <p
          key={result.path}
          className={
            result.error
              ? "break-all text-amber-500"
              : "break-all text-content/55"
          }
        >
          {result.path}:{" "}
          {result.error ?? "cleared; selected configuration kept"}
        </p>
      ))}
      {interrupted && report.selectedPaths?.length ? (
        <p className="break-all text-content/55">
          Selected directories: {report.selectedPaths.join(", ")}
        </p>
      ) : null}
      {interrupted ? (
        <p className="text-amber-500">
          Deletion will not resume automatically after interruption. Review
          remaining outputs before another cleanup.
        </p>
      ) : null}
      {report.preparationNeeded ? (
        <p className="text-content/60">
          Workspace preparation needed before coding, files or terminals. Retry
          the workspace action if setup fails.
        </p>
      ) : null}
    </div>
  );
}
