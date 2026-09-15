import { useEffect, useState } from "react";
import {
  DEFAULT_RETIREMENT_POLICY,
  listWorktrees,
  isRetirementPolicyConflict,
  retryAutomaticRetirement,
  saveRetirementPolicy,
  type AutomaticRetirement,
  type WorktreeRetirementPolicy,
} from "../lib/worktrees";
import { prettyCwd } from "../lib/paths";

const button =
  "rounded-md border border-content/10 px-2.5 py-1.5 text-[12px] text-content/70 hover:bg-content/8 disabled:opacity-40";
const labels: Record<AutomaticRetirement["status"], string> = {
  pending: "Pending",
  blocked: "Protected",
  failed: "Cleanup needs attention",
  paused: "Paused · manual review",
  complete: "Checkout retired · recovery saved",
};

export function WorktreeAutomaticRetirement({
  cwd,
  policy = DEFAULT_RETIREMENT_POLICY,
  items = [],
  disabled = false,
  onSaved,
}: {
  cwd: string;
  policy?: WorktreeRetirementPolicy;
  items?: AutomaticRetirement[];
  disabled?: boolean;
  onSaved: () => Promise<void> | void;
}) {
  const [baseline, setBaseline] = useState(policy);
  const [mode, setMode] = useState(policy.mode);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [saveConflict, setSaveConflict] = useState(false);
  const dirty = mode !== baseline.mode;
  const newer = policy.version > baseline.version;
  const conflict = saveConflict || (dirty && newer);

  useEffect(() => {
    if (!dirty && !busy && newer) {
      setBaseline(policy);
      setMode(policy.mode);
      setSaved(false);
    }
  }, [policy, dirty, busy, newer]);

  async function perform(operation: () => Promise<void>) {
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      await operation();
    } catch (cause) {
      setError(
        (cause instanceof Error ? cause.message : String(cause)).replace(
          /^WORKTREE_RETIREMENT_CONFLICT:\s*/,
          "",
        ),
      );
    } finally {
      setBusy(false);
    }
  }

  const outstanding = items.filter((item) => item.status !== "complete");
  const completed = items.filter((item) => item.status === "complete");
  const rows = (entries: AutomaticRetirement[]) =>
    entries.map((item) => (
      <li key={item.id} className="py-2.5">
        <p className="break-all text-[12px] text-content/75">
          {prettyCwd(item.path)}
        </p>
        <p className="mt-1 text-[11px] font-medium text-content/55">
          {labels[item.status]}
        </p>
        {item.reason ? (
          <p className="mt-1 break-words text-[12px] text-content/60">
            {item.reason}
          </p>
        ) : null}
      </li>
    ));

  return (
    <section
      aria-label="Archive cleanup"
      className="space-y-3 rounded-lg border border-content/10 p-4"
    >
      <div>
        <h2 className="text-[13px] font-medium">After archiving</h2>
        <p className="mt-1 text-[12px] leading-relaxed text-content/50">
          Choose what happens after the last conversation in a worktree is
          archived. This preference applies to this project, including its
          already archived conversations.
        </p>
      </div>
      <label className="block text-[12px] text-content/70">
        Retirement preference
        <select
          aria-label="Retirement preference"
          value={mode}
          disabled={disabled || busy}
          onChange={(event) => {
            setMode(event.target.value as WorktreeRetirementPolicy["mode"]);
            setSaved(false);
          }}
          className="mt-1.5 block w-full rounded-md border border-content/10 bg-surface px-2.5 py-2 text-[12px]"
        >
          <option value="manual">Manual review (default)</option>
          <option value="automatic">
            Automatically retire eligible worktrees
          </option>
        </select>
      </label>
      <p className="text-[12px] leading-relaxed text-content/50">
        Automatic retirement removes clean, recoverable working folders and
        generated files. It preserves committed code, selected local
        configuration, conversation history and both branches. Active work,
        pins, setup, Git locks, uncommitted changes and unknown files keep a
        checkout protected.
      </p>
      <p className="text-[11px] text-content/45">
        Pending cleanup is retried after startup, every five minutes and when
        relevant activity or preferences change. Saving manual review pauses
        automatic removals that have not started.
      </p>
      {conflict ? (
        <p role="alert" className="text-[12px] text-amber-500">
          The saved preference changed in another window. Your draft is kept;
          reload before saving.
        </p>
      ) : null}
      <div className="flex flex-wrap items-center gap-2">
        <button
          className={button}
          disabled={disabled || busy || !dirty || conflict}
          onClick={() =>
            void perform(async () => {
              let next: WorktreeRetirementPolicy;
              try {
                next = await saveRetirementPolicy(cwd, { ...baseline, mode });
              } catch (cause) {
                setSaveConflict(isRetirementPolicyConflict(cause));
                throw cause;
              }
              setBaseline(next);
              setMode(next.mode);
              setSaveConflict(false);
              setSaved(true);
              await onSaved();
            })
          }
        >
          Save retirement preference
        </button>
        {conflict || error ? (
          <button
            className={button}
            disabled={busy || disabled}
            onClick={() =>
              void perform(async () => {
                const latest =
                  (await listWorktrees(cwd)).retirementPolicy ??
                  DEFAULT_RETIREMENT_POLICY;
                setBaseline(latest);
                setMode(latest.mode);
                setSaveConflict(false);
                await onSaved();
              })
            }
          >
            Reload preference
          </button>
        ) : null}
        {baseline.mode === "automatic" && outstanding.length > 0 ? (
          <button
            className={button}
            disabled={busy || disabled}
            onClick={() =>
              void perform(async () => {
                await retryAutomaticRetirement();
                await onSaved();
              })
            }
          >
            Retry automatic cleanup
          </button>
        ) : null}
      </div>
      {saved ? (
        <p role="status" className="text-[12px] text-content/55">
          Retirement preference saved.
        </p>
      ) : null}
      {busy ? (
        <p role="status" className="text-[12px] text-content/55">
          Checking…
        </p>
      ) : null}
      {error ? (
        <p role="alert" className="text-[12px] text-red-400">
          {error}
        </p>
      ) : null}
      {outstanding.length > 0 ? (
        <div className="border-t border-content/10 pt-3">
          <h3 className="text-[12px] font-medium">
            Pending archive cleanup ({outstanding.length})
          </h3>
          <ul
            aria-label="Pending archive cleanup"
            className="mt-1 divide-y divide-content/5"
          >
            {rows(outstanding)}
          </ul>
        </div>
      ) : null}
      {completed.length > 0 ? (
        <details className="border-t border-content/10 pt-3">
          <summary className="cursor-pointer text-[12px] text-content/55">
            Retired after archiving ({completed.length})
          </summary>
          <ul className="mt-1 divide-y divide-content/5">{rows(completed)}</ul>
        </details>
      ) : null}
    </section>
  );
}
