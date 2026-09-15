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
import { Group, Row, SecondaryButton, Select } from "./SettingsControls";
import { ChevronRight } from "./icons";

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
  const [historyOpen, setHistoryOpen] = useState(false);
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
      setHistoryOpen(true);
    } catch (cause) {
      setError(`${message(cause)}. Review again before retrying.`);
      // A lost response may follow partial deletion. The durable journal is
      // authoritative, and this review must never offer a blind execute retry.
      setReview(null);
      setHistoryOpen(true);
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
    <Group
      title="Generated files"
      description="Free space in an idle worktree while keeping its source, branches and conversations."
    >
      <div role="group" aria-label="Clear generated files">
        <Row
          label="Clear generated files"
          description={
            checkouts.length
              ? "Review disposable folders before clearing them."
              : "No managed checkouts available for output review."
          }
        >
          {checkouts.length ? (
            <>
              <Select
                label="Worktree to clear"
                value={id}
                disabled={disabled || busy || !!review}
                onChange={(value) => {
                  setChosen(value);
                  setError(null);
                }}
                options={checkouts.map((entry) => ({
                  value: entry.id!,
                  label: entry.branch ?? entry.path,
                }))}
              />
              <SecondaryButton
                disabled={disabled || busy || !!review || !id}
                onClick={() => void inspect()}
              >
                Review generated files
              </SecondaryButton>
            </>
          ) : null}
        </Row>
        <details className="group/output-protection border-t border-content/5 px-4 py-3">
          <summary className="flex cursor-pointer list-none items-center gap-2 text-[12px] text-content/45 hover:text-content/70 focus-visible:outline-1 focus-visible:outline-accent [&::-webkit-details-marker]:hidden">
            <ChevronRight className="size-3 shrink-0 transition-transform group-open/output-protection:rotate-90" />
            What cleanup preserves
          </summary>
          <p className="mt-2 text-[12px] leading-relaxed text-content/45">
            Only recognized or configured disposable folders are offered.
            Selected local configuration stays in place, including files inside
            output folders. Live workspaces, pins, agents, terminals, setup and
            Git locks block cleanup. An idle unarchived conversation alone does
            not.
          </p>
        </details>
        {review ? (
          <div
            aria-label="Output cleanup review"
            className="space-y-3 border-t border-content/5 px-4 py-3.5"
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
                      className="mt-0.5 size-3.5 shrink-0 accent-accent"
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
              estimated for selected files. Shared storage and other disk
              activity affect actual free space.
            </p>
            <div className="flex flex-wrap gap-2">
              <SecondaryButton
                danger
                disabled={
                  disabled || busy || !!review.blockedReason || !eligible.length
                }
                onClick={() => void execute()}
              >
                Clear selected outputs ({eligible.length})
              </SecondaryButton>
              <SecondaryButton
                disabled={busy}
                onClick={() => {
                  setReview(null);
                  setSelection([]);
                }}
              >
                Cancel review
              </SecondaryButton>
            </div>
          </div>
        ) : null}
        {busy ? (
          <p role="status" className="px-4 py-3 text-[12px] text-content/55">
            Checking generated files…
          </p>
        ) : null}
        {error ? (
          <p role="alert" className="px-4 py-3 text-[12px] text-red-400">
            {error}
          </p>
        ) : null}
        {historyError ? (
          <p role="alert" className="px-4 py-3 text-[12px] text-amber-500">
            Cleanup history or usage refresh failed: {historyError}
          </p>
        ) : null}
        {reports.length ? (
          <details
            open={historyOpen}
            onToggle={(event) => setHistoryOpen(event.currentTarget.open)}
            className="group/output-history border-t border-content/5 px-4 py-3"
          >
            <summary className="flex cursor-pointer list-none items-center gap-2 text-[12px] text-content/45 hover:text-content/70 focus-visible:outline-1 focus-visible:outline-accent [&::-webkit-details-marker]:hidden">
              <ChevronRight className="size-3 shrink-0 transition-transform group-open/output-history:rotate-90" />
              Recent output cleanup
            </summary>
            <div className="mt-2 space-y-4">
              {reports.slice(0, 5).map((report) => (
                <CleanupResult key={report.planId} report={report} />
              ))}
            </div>
          </details>
        ) : null}
      </div>
    </Group>
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
