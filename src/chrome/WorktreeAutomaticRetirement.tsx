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
import { ChevronRight } from "./icons";
import { prettyCwd } from "../lib/paths";
import { Group, Row, SecondaryButton, Segmented } from "./SettingsControls";

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
    <Group
      title="After archiving"
      description="Choose what happens after the last conversation in a worktree is archived."
    >
      <Row
        label="Retirement preference"
        description="Applies to this project, including already archived conversations."
      >
        <Segmented
          label="Retirement preference"
          value={mode}
          disabled={disabled || busy}
          options={[
            { value: "manual", label: "Manual review" },
            { value: "automatic", label: "Automatic" },
          ]}
          onChange={(value) => {
            setMode(value);
            setSaved(false);
          }}
        />
      </Row>
      <div className="space-y-2 border-b border-content/5 px-4 py-3.5">
        <p className="text-[12px] leading-relaxed text-content/45">
          Automatic retirement removes clean, recoverable worktree folders.
          Committed code, selected local files, conversations and branches are
          preserved.
        </p>
        <details className="group/retirement">
          <summary className="flex cursor-pointer list-none items-center gap-2 text-[12px] text-content/50 hover:text-content focus-visible:outline-1 focus-visible:outline-accent [&::-webkit-details-marker]:hidden">
            <ChevronRight className="size-3 shrink-0 transition-transform group-open/retirement:rotate-90" />
            How automatic retirement works
          </summary>
          <div className="mt-2 space-y-2 text-[12px] leading-relaxed text-content/45">
            <p>
              Active work, pins, setup, Git locks, uncommitted changes and
              unknown files keep a checkout protected. Eligible working folders
              and generated files are removed after recovery is saved.
            </p>
            <p>
              Pending cleanup is retried after startup, every five minutes and
              when relevant activity or preferences change. Saving manual review
              pauses automatic removals that have not started.
            </p>
          </div>
        </details>
      </div>
      <div className="flex flex-wrap items-center justify-between gap-3 border-b border-content/5 px-4 py-3.5 last:border-b-0">
        <div className="min-w-0 flex-1 space-y-2">
          {conflict ? (
            <p role="alert" className="text-[12px] text-content/60">
              The saved preference changed in another window. Your draft is
              kept; reload before saving.
            </p>
          ) : saved ? (
            <p role="status" className="text-[12px] text-content/45">
              Retirement preference saved.
            </p>
          ) : busy ? (
            <p role="status" className="text-[12px] text-content/45">
              Checking…
            </p>
          ) : (
            <p className="text-[12px] text-content/45">
              Save to apply this preference.
            </p>
          )}
          {error ? (
            <p role="alert" className="text-[12px] text-red-400">
              {error}
            </p>
          ) : null}
          {conflict || error ? (
            <SecondaryButton
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
            </SecondaryButton>
          ) : null}
        </div>
        <SecondaryButton
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
        </SecondaryButton>
      </div>
      {outstanding.length > 0 ? (
        <details className="group/retirement border-b border-content/5 px-4 py-3.5 last:border-b-0">
          <summary className="flex cursor-pointer list-none items-center gap-2 text-[12px] text-content/50 hover:text-content focus-visible:outline-1 focus-visible:outline-accent [&::-webkit-details-marker]:hidden">
            <ChevronRight className="size-3 shrink-0 transition-transform group-open/retirement:rotate-90" />
            Pending archive cleanup ({outstanding.length})
          </summary>
          <ul
            aria-label="Pending archive cleanup"
            className="mt-1 divide-y divide-content/5"
          >
            {rows(outstanding)}
          </ul>
          {baseline.mode === "automatic" ? (
            <div className="pt-2">
              <SecondaryButton
                disabled={busy || disabled}
                onClick={() =>
                  void perform(async () => {
                    await retryAutomaticRetirement();
                    await onSaved();
                  })
                }
              >
                Retry automatic cleanup
              </SecondaryButton>
            </div>
          ) : null}
        </details>
      ) : null}
      {completed.length > 0 ? (
        <details className="group/retirement px-4 py-3.5">
          <summary className="flex cursor-pointer list-none items-center gap-2 text-[12px] text-content/50 hover:text-content focus-visible:outline-1 focus-visible:outline-accent [&::-webkit-details-marker]:hidden">
            <ChevronRight className="size-3 shrink-0 transition-transform group-open/retirement:rotate-90" />
            Retired after archiving ({completed.length})
          </summary>
          <ul className="mt-1 divide-y divide-content/5">{rows(completed)}</ul>
        </details>
      ) : null}
    </Group>
  );
}
