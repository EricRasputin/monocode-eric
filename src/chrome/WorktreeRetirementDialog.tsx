import { useEffect, useId, useRef, useState } from "react";
import { prettyCwd, projectName } from "../lib/paths";
import {
  retireWorktrees,
  type WorktreeRetirementEntry,
  type WorktreeRetirementPlan,
  type WorktreeRetirementReport,
  type WorktreeRetirementSelection,
} from "../lib/worktrees";
import { Check, CircleAlert, Worktree, Loader } from "./icons";
import { Modal } from "./Modal";

export type WorktreeRetirementDialogProps = {
  plan: WorktreeRetirementPlan;
  source: "archive" | "settings";
  onClose: () => void;
  onRetired: (report: WorktreeRetirementReport) => void;
};

type SelectionById = Record<
  string,
  { deleteLocalBranch: boolean; deleteRemoteBranch: boolean }
>;

const quietButton =
  "rounded-md px-3 py-1.5 text-[12px] text-content/60 hover:bg-content/8 hover:text-content focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:cursor-default disabled:opacity-40";
const retireButton =
  "inline-flex items-center justify-center gap-1.5 rounded-md border border-red-400/20 bg-red-400/10 px-3 py-1.5 text-[12px] font-medium text-red-300 hover:bg-red-400/15 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-red-400/60 disabled:cursor-default disabled:opacity-40";
const focusableSelector =
  'button:not([disabled]), input:not([disabled]), [href], [tabindex]:not([tabindex="-1"])';

function initialSelections(plan: WorktreeRetirementPlan): SelectionById {
  return Object.fromEntries(
    plan.entries.map((entry) => [
      entry.id,
      { deleteLocalBranch: false, deleteRemoteBranch: false },
    ]),
  );
}

function selectionsFor(
  entries: WorktreeRetirementEntry[],
  selected: SelectionById,
): WorktreeRetirementSelection[] {
  return entries
    .filter((entry) => !entry.blockedReason)
    .map((entry) => ({ id: entry.id, ...selected[entry.id] }));
}

function mergeReports(
  previous: WorktreeRetirementReport | null,
  next: WorktreeRetirementReport,
): WorktreeRetirementReport {
  if (!previous) return next;
  const nextById = new Map(next.results.map((result) => [result.id, result]));
  const results = previous.results.map((result) => {
    const update = nextById.get(result.id);
    if (!update) return result;
    nextById.delete(result.id);
    // Each result includes the durable operation's current state. A checkout or
    // branch may have been restored since the previous attempt.
    return update;
  });
  return { results: [...results, ...nextById.values()] };
}

function unfinishedSelections(
  selections: WorktreeRetirementSelection[],
  report: WorktreeRetirementReport | null,
): WorktreeRetirementSelection[] {
  if (!report) return selections;
  const results = new Map(report.results.map((result) => [result.id, result]));
  return selections.flatMap((selection) => {
    const result = results.get(selection.id);
    if (!result) return [selection];
    const deleteLocalBranch =
      selection.deleteLocalBranch && !result.localBranchDeleted;
    const deleteRemoteBranch =
      selection.deleteRemoteBranch && !result.remoteBranchDeleted;
    if (result.worktreeRemoved && !deleteLocalBranch && !deleteRemoteBranch) {
      return [];
    }
    return [{ ...selection, deleteLocalBranch, deleteRemoteBranch }];
  });
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function WorktreeRetirementDialog({
  plan,
  source,
  onClose,
  onRetired,
}: WorktreeRetirementDialogProps) {
  const [selected, setSelected] = useState<SelectionById>(() =>
    initialSelections(plan),
  );
  const [busy, setBusy] = useState(false);
  const [report, setReport] = useState<WorktreeRetirementReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const controlsId = useId();
  const contentRef = useRef<HTMLDivElement>(null);
  const safeActionRef = useRef<HTMLButtonElement>(null);
  const originRef = useRef<HTMLElement | null>(
    typeof document !== "undefined" &&
      document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null,
  );
  const actionable = plan.entries.filter((entry) => !entry.blockedReason);
  const selections = selectionsFor(actionable, selected);
  const unfinished = unfinishedSelections(selections, report);
  const reviewedCount = plan.entries.length;
  const alreadyRemovedCount = actionable.filter(
    (entry) => entry.worktreeRemoved,
  ).length;
  const title =
    reviewedCount === 0
      ? "Worktree kept"
      : reviewedCount === 1
        ? "Retire worktree?"
        : `Retire ${reviewedCount} worktrees?`;

  useEffect(() => {
    const dialog = contentRef.current?.closest<HTMLElement>('[role="dialog"]');
    if (!dialog) return;
    const keepFocusInside = (event: KeyboardEvent) => {
      if (event.key !== "Tab") return;
      const focusable = [
        ...dialog.querySelectorAll<HTMLElement>(focusableSelector),
      ];
      if (!focusable.length) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      } else if (!dialog.contains(document.activeElement)) {
        event.preventDefault();
        first.focus();
      }
    };
    dialog.addEventListener("keydown", keepFocusInside);
    safeActionRef.current?.focus();
    const origin = originRef.current;
    return () => {
      dialog.removeEventListener("keydown", keepFocusInside);
      if (origin?.isConnected) origin.focus();
    };
  }, []);

  useEffect(() => {
    safeActionRef.current?.focus();
  }, [report]);

  useEffect(() => {
    const close = contentRef.current
      ?.closest<HTMLElement>('[role="dialog"]')
      ?.querySelector<HTMLButtonElement>('button[aria-label="Close"]');
    if (close) {
      close.disabled = busy;
      close.classList.toggle("cursor-default", busy);
      close.classList.toggle("opacity-40", busy);
    }
    return () => {
      if (close) {
        close.disabled = false;
        close.classList.remove("cursor-default", "opacity-40");
      }
    };
  }, [busy]);

  const execute = async (nextSelections: WorktreeRetirementSelection[]) => {
    setBusy(true);
    setError(null);
    try {
      const next = await retireWorktrees(plan.planId, nextSelections);
      const merged = mergeReports(report, next);
      setReport(merged);
      onRetired(merged);
    } catch (cause) {
      setError(message(cause));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      title={title}
      description={
        source === "archive"
          ? "The conversation is archived. Review what Monocode can remove."
          : "Review what Monocode can remove from this project."
      }
      onClose={busy ? () => undefined : onClose}
      className="max-h-[80vh]"
    >
      <div ref={contentRef} className="space-y-4 px-4 pb-4 pt-3">
        {!report ? (
          <p className="text-[12px] leading-relaxed text-content/55">
            {alreadyRemovedCount === actionable.length && actionable.length > 0
              ? "The working folder is already removed."
              : alreadyRemovedCount > 0
                ? "Working folders marked below are already removed. The other working folders and their approved generated folders will be removed."
                : "The working folder and approved generated folders will be removed."}{" "}
            Monocode preserves your committed code and configured local files so
            this conversation can be restored. Branches stay unless you select
            them below.
          </p>
        ) : null}

        <div className="space-y-2">
          {plan.entries.map((entry, index) => (
            <RetirementEntry
              key={entry.id}
              entry={entry}
              inputId={`${controlsId}-${index}`}
              selected={selected[entry.id]}
              result={report?.results.find((result) => result.id === entry.id)}
              disabled={busy || !!report}
              onChange={(choice, checked) =>
                setSelected((current) => ({
                  ...current,
                  [entry.id]: { ...current[entry.id], [choice]: checked },
                }))
              }
            />
          ))}
        </div>

        {plan.kept.length > 0 ? (
          <section
            aria-label="Worktrees kept"
            className="rounded-lg border border-content/10 bg-content/3 p-3"
          >
            <p className="text-[11px] font-medium uppercase tracking-wide text-content/40">
              Kept for safety
            </p>
            <div className="mt-2 space-y-2">
              {plan.kept.map((item) => (
                <div key={item.id} className="text-[12px]">
                  <p className="truncate text-content/70" title={item.path}>
                    {prettyCwd(item.path)}
                  </p>
                  <p className="mt-0.5 text-[11px] text-content/45">
                    {item.reason}
                  </p>
                </div>
              ))}
            </div>
          </section>
        ) : null}

        {busy ? (
          <p
            role="status"
            aria-live="polite"
            className="flex items-center gap-2 text-[12px] text-content/55"
          >
            <Loader className="size-3.5 animate-spin" aria-hidden />
            Retiring worktree{actionable.length === 1 ? "" : "s"}…
          </p>
        ) : null}
        {error ? (
          <p
            role="alert"
            className="rounded-md border border-red-400/20 bg-red-400/5 px-3 py-2 text-[12px] text-red-300"
          >
            {error}
          </p>
        ) : null}

        <div className="flex items-center justify-end gap-2 border-t border-content/10 pt-3">
          {report ? (
            <>
              {unfinished.length > 0 ? (
                <button
                  type="button"
                  className={retireButton}
                  disabled={busy}
                  onClick={() => void execute(unfinished)}
                >
                  {busy ? (
                    <Loader className="size-3.5 animate-spin" aria-hidden />
                  ) : null}
                  Retry unfinished work
                </button>
              ) : null}
              <button
                ref={safeActionRef}
                type="button"
                className={quietButton}
                disabled={busy}
                onClick={onClose}
              >
                Done
              </button>
            </>
          ) : (
            <>
              <button
                ref={safeActionRef}
                type="button"
                className={quietButton}
                disabled={busy}
                onClick={onClose}
              >
                Keep for now
              </button>
              {actionable.length > 0 ? (
                <button
                  type="button"
                  className={retireButton}
                  disabled={busy}
                  onClick={() => void execute(selections)}
                >
                  {busy ? (
                    <Loader className="size-3.5 animate-spin" aria-hidden />
                  ) : null}
                  {actionable.length === 1
                    ? "Retire worktree"
                    : `Retire ${actionable.length} worktrees`}
                </button>
              ) : null}
            </>
          )}
        </div>
      </div>
    </Modal>
  );
}

function RetirementEntry({
  entry,
  inputId,
  selected,
  result,
  disabled,
  onChange,
}: {
  entry: WorktreeRetirementEntry;
  inputId: string;
  selected: SelectionById[string];
  result?: WorktreeRetirementReport["results"][number];
  disabled: boolean;
  onChange: (
    choice: "deleteLocalBranch" | "deleteRemoteBranch",
    checked: boolean,
  ) => void;
}) {
  const remote = entry.remoteBranch;
  return (
    <section className="rounded-lg border border-content/10 bg-content/3 p-3">
      <div className="flex items-start gap-2.5">
        <Worktree
          className="mt-0.5 size-4 shrink-0 text-content/40"
          strokeWidth={1.75}
          aria-hidden
        />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-0.5">
            <p className="break-all font-mono text-[12px] font-medium text-content/85">
              {entry.branch}
            </p>
            <span className="text-[10px] text-content/35">
              {projectName(entry.repo)}
            </span>
          </div>
          <p
            title={entry.path}
            className="mt-1 truncate text-[11px] text-content/40"
          >
            {prettyCwd(entry.path)}
          </p>
        </div>
      </div>

      {entry.blockedReason ? (
        <p className="mt-2 flex items-start gap-1.5 rounded-md bg-content/5 px-2.5 py-2 text-[11px] leading-relaxed text-content/55">
          <CircleAlert className="mt-px size-3.5 shrink-0" aria-hidden />
          Kept for safety: {entry.blockedReason}
        </p>
      ) : result ? (
        <RetirementResult entry={entry} selected={selected} result={result} />
      ) : (
        <>
          {entry.worktreeRemoved ? (
            <p className="mt-2 flex items-center gap-1.5 text-[11px] text-content/50">
              <Check
                className="size-3.5 shrink-0 text-emerald-400"
                aria-hidden
              />
              Working folder already removed. Finish any branch removal below.
            </p>
          ) : null}
          <fieldset className="mt-3 space-y-2 border-t border-content/8 pt-2.5">
            <legend className="sr-only">
              Branch options for {entry.branch}
            </legend>
            <BranchChoice
              id={`${inputId}-local`}
              label="Delete local branch"
              name={entry.localBranch.name}
              checked={selected.deleteLocalBranch}
              disabled={disabled || !entry.localBranch.allowed}
              reason={entry.localBranch.reason}
              onChange={(checked) => onChange("deleteLocalBranch", checked)}
            />
            {remote ? (
              <BranchChoice
                id={`${inputId}-remote`}
                label="Delete remote branch"
                name={remote.name}
                detail={`${remote.remote} → ${remote.destination}`}
                checked={selected.deleteRemoteBranch}
                disabled={disabled || !remote.allowed}
                reason={remote.reason}
                onChange={(checked) => onChange("deleteRemoteBranch", checked)}
              />
            ) : (
              <p className="pl-5 text-[11px] text-content/35">
                No remote branch is linked to this worktree.
              </p>
            )}
          </fieldset>
        </>
      )}
    </section>
  );
}

function BranchChoice({
  id,
  label,
  name,
  detail,
  checked,
  disabled,
  reason,
  onChange,
}: {
  id: string;
  label: string;
  name: string;
  detail?: string;
  checked: boolean;
  disabled: boolean;
  reason: string | null;
  onChange: (checked: boolean) => void;
}) {
  const reasonId = reason ? `${id}-reason` : undefined;
  return (
    <div>
      <label
        htmlFor={id}
        className={`flex items-start gap-2 text-[11px] ${disabled ? "cursor-default text-content/35" : "cursor-pointer text-content/65"}`}
      >
        <input
          id={id}
          type="checkbox"
          checked={checked}
          disabled={disabled}
          aria-describedby={reasonId}
          onChange={(event) => onChange(event.target.checked)}
          className="mt-0.5 size-3.5 shrink-0 accent-accent"
        />
        <span className="min-w-0">
          {label} <code className="break-all text-content/80">{name}</code>
          {detail ? (
            <span className="mt-0.5 block break-all text-[10px] text-content/40">
              {detail}
            </span>
          ) : null}
        </span>
      </label>
      {reason ? (
        <p id={reasonId} className="mt-0.5 pl-5 text-[10px] text-content/40">
          {reason}
        </p>
      ) : null}
    </div>
  );
}

function RetirementResult({
  entry,
  selected,
  result,
}: {
  entry: WorktreeRetirementEntry;
  selected: SelectionById[string];
  result: WorktreeRetirementReport["results"][number];
}) {
  const pieces = [
    result.worktreeRemoved ? "Working folder removed." : "Working folder kept.",
    selected.deleteLocalBranch
      ? result.localBranchDeleted
        ? "Local branch deleted."
        : "Local deletion not confirmed."
      : "Local branch kept.",
    entry.remoteBranch
      ? selected.deleteRemoteBranch
        ? result.remoteBranchDeleted
          ? "Remote branch deleted."
          : "Remote deletion not confirmed."
        : "Remote branch kept."
      : null,
  ].filter(Boolean);
  const complete =
    result.worktreeRemoved &&
    (!selected.deleteLocalBranch || result.localBranchDeleted) &&
    (!selected.deleteRemoteBranch || result.remoteBranchDeleted);
  return (
    <div
      role={result.error || !complete ? "alert" : "status"}
      className={`mt-2 rounded-md border px-2.5 py-2 text-[11px] leading-relaxed ${
        complete
          ? "border-emerald-400/15 bg-emerald-400/5 text-content/60"
          : "border-amber-400/20 bg-amber-400/5 text-content/65"
      }`}
    >
      <p className="flex items-start gap-1.5">
        {complete ? (
          <Check className="mt-px size-3.5 shrink-0 text-emerald-400" />
        ) : (
          <CircleAlert className="mt-px size-3.5 shrink-0 text-amber-400" />
        )}
        <span>{pieces.join(" ")}</span>
      </p>
      {result.worktreeRemoved && result.recoveryRef ? (
        <p className="mt-1 pl-5 text-content/45">
          Monocode preserved the committed code. Reopen the archived
          conversation to restore its configured local files and run setup
          again.
        </p>
      ) : !result.worktreeRemoved ? (
        <p className="mt-1 pl-5 text-content/45">
          Files remain in the checkout.
        </p>
      ) : null}
      {result.error ? (
        <p className="mt-1 pl-5 text-red-300">{result.error}</p>
      ) : null}
    </div>
  );
}
