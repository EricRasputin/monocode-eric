import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useCallback, useEffect, useRef, useState } from "react";
import { projectKey } from "../lib/paths";
import {
  formatRecoveryStorageBytes,
  getRecoveryStorage,
  MAX_RECOVERY_STORAGE_MIB,
  MIN_RECOVERY_STORAGE_MIB,
  RECOVERY_STORAGE_MIB,
  recoveryStoragePressure,
  setRecoveryStorageLimit,
  type RecoveryStorageUsage,
} from "../lib/worktreeStorage";
import { ChevronRight, Loader } from "./icons";
import { Group, Row, SecondaryButton } from "./SettingsControls";

type SavedLimit = { limitBytes: number; version: number };

const input =
  "w-24 rounded-md border border-content/10 bg-content/3 px-2.5 py-1.5 text-right text-[12px] tabular-nums text-content/75 outline-none focus:border-accent/60 focus:ring-1 focus:ring-accent/30 disabled:cursor-default disabled:opacity-50";

function limitDraft(limitBytes: number): string {
  return String(Math.round(limitBytes / RECOVERY_STORAGE_MIB));
}

function errorText(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

export function WorktreeRecoveryStorage({
  projectCwd,
}: {
  projectCwd: string;
}) {
  const [usage, setUsage] = useState<RecoveryStorageUsage | null>(null);
  const [baseline, setBaseline] = useState<SavedLimit | null>(null);
  const [draft, setDraft] = useState("");
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [reloading, setReloading] = useState(false);
  const [conflict, setConflict] = useState(false);
  const [feedback, setFeedback] = useState<
    { kind: "saved" | "error"; text: string } | undefined
  >();
  const baselineRef = useRef<SavedLimit | null>(null);
  const dirtyRef = useRef(false);
  const hasUsageRef = useRef(false);
  const operationRef = useRef(0);
  const latestUsageRef = useRef<RecoveryStorageUsage | null>(null);
  const latestAppliedRef = useRef({ version: -1, operation: -1 });

  const applyUsage = useCallback(
    (
      next: RecoveryStorageUsage,
      adoptLimit: boolean,
      operation: number,
    ): boolean => {
      const latest = latestAppliedRef.current;
      if (
        next.version < latest.version ||
        (next.version === latest.version && operation < latest.operation)
      ) {
        return false;
      }
      latestAppliedRef.current = { version: next.version, operation };
      latestUsageRef.current = next;
      setUsage(next);
      hasUsageRef.current = true;
      const currentBaseline = baselineRef.current;
      if (adoptLimit || !currentBaseline || !dirtyRef.current) {
        const nextBaseline = {
          limitBytes: next.limitBytes,
          version: next.version,
        };
        baselineRef.current = nextBaseline;
        dirtyRef.current = false;
        setBaseline(nextBaseline);
        setDraft(limitDraft(next.limitBytes));
        setConflict(false);
      } else if (
        next.version !== currentBaseline.version ||
        next.limitBytes !== currentBaseline.limitBytes
      ) {
        setConflict(true);
      }
      return true;
    },
    [],
  );

  const load = useCallback(
    async (adoptLimit = false) => {
      const operation = ++operationRef.current;
      if (!hasUsageRef.current) setLoading(true);
      try {
        const next = await getRecoveryStorage();
        if (operation !== operationRef.current) return false;
        if (!applyUsage(next, adoptLimit, operation)) return false;
        setFeedback(undefined);
        return true;
      } catch (cause) {
        if (operation !== operationRef.current) return false;
        setFeedback({ kind: "error", text: errorText(cause) });
        return false;
      } finally {
        if (operation === operationRef.current) setLoading(false);
      }
    },
    [applyUsage],
  );

  useEffect(() => {
    let disposed = false;
    let unlisten: UnlistenFn | undefined;
    void load(true);

    const refresh = () => void load();
    window.addEventListener("focus", refresh);
    void listen("worktree-storage-changed", refresh)
      .then((stop) => {
        if (disposed) stop();
        else unlisten = stop;
      })
      .catch(() => undefined);

    return () => {
      disposed = true;
      operationRef.current += 1;
      window.removeEventListener("focus", refresh);
      unlisten?.();
    };
  }, [load]);

  const reloadSavedLimit = async () => {
    setReloading(true);
    await load(true);
    setReloading(false);
  };

  const parsedDraft = Number(draft.trim());
  const validDraft =
    /^\d+$/.test(draft.trim()) &&
    Number.isInteger(parsedDraft) &&
    parsedDraft >= MIN_RECOVERY_STORAGE_MIB &&
    parsedDraft <= MAX_RECOVERY_STORAGE_MIB;
  const dirty = !!baseline && draft !== limitDraft(baseline.limitBytes);

  const save = async () => {
    if (!baseline || !dirty || !validDraft || saving || conflict) return;
    const operation = ++operationRef.current;
    setSaving(true);
    setFeedback(undefined);
    try {
      const saved = await setRecoveryStorageLimit(
        parsedDraft * RECOVERY_STORAGE_MIB,
        baseline.version,
      );
      const latest = latestUsageRef.current;
      const applied =
        latest &&
        latest.version === saved.version &&
        latest.limitBytes === saved.limitBytes
          ? applyUsage(latest, true, latestAppliedRef.current.operation)
          : applyUsage(saved, true, operation);
      if (applied) {
        setFeedback({ kind: "saved", text: "Storage limit saved." });
      } else if (latest && latest.version > saved.version) {
        setConflict(true);
      }
    } catch (cause) {
      const text = errorText(cause);
      if (text.includes("WORKTREE_STORAGE_CONFLICT")) {
        setConflict(true);
      } else {
        setFeedback({ kind: "error", text });
      }
    } finally {
      setSaving(false);
    }
  };

  const pressure = usage
    ? recoveryStoragePressure(usage.usedBytes, usage.limitBytes)
    : "normal";
  const pressureLabel =
    pressure === "full"
      ? "Storage full"
      : pressure === "warning"
        ? "Near limit"
        : null;
  const pressureColor =
    pressure === "full"
      ? "text-red-400"
      : pressure === "warning"
        ? "text-amber-400"
        : "text-content/35";
  const project = projectKey(projectCwd);
  const projectBytes =
    usage?.projects.reduce(
      (total, item) =>
        projectKey(item.projectCwd) === project
          ? total + item.usedBytes
          : total,
      0,
    ) ?? 0;
  const percentage = usage?.limitBytes
    ? Math.min(100, Math.max(0, (usage.usedBytes / usage.limitBytes) * 100))
    : 0;

  return (
    <Group
      title="Recovery storage"
      description="Space for saved local configuration across all projects."
    >
      {usage ? (
        <>
          <Row
            label="Storage used"
            description={`This project: ${formatRecoveryStorageBytes(projectBytes)}`}
          >
            <div className="w-56 max-w-full space-y-2">
              <div className="text-right text-[12px] text-content/70">
                {formatRecoveryStorageBytes(usage.usedBytes)} of{" "}
                {formatRecoveryStorageBytes(usage.limitBytes)} used app-wide
              </div>
              <div
                role="meter"
                aria-label="Recovery storage used"
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={Math.round(percentage)}
                className="h-1 overflow-hidden rounded-full bg-content/8"
              >
                <div
                  className={`h-full rounded-full ${
                    pressure === "full"
                      ? "bg-red-400"
                      : pressure === "warning"
                        ? "bg-amber-400"
                        : "bg-content/30"
                  }`}
                  style={{ width: `${percentage}%` }}
                />
              </div>
              {pressureLabel ? (
                <p className={`text-right text-[11px] ${pressureColor}`}>
                  {pressureLabel}
                </p>
              ) : null}
            </div>
          </Row>
          <Row
            label="App-wide limit"
            description="Maximum space available for configuration recovery."
          >
            <input
              aria-label="Recovery storage limit in MB"
              type="number"
              inputMode="numeric"
              min={MIN_RECOVERY_STORAGE_MIB}
              max={MAX_RECOVERY_STORAGE_MIB}
              step={1}
              value={draft}
              disabled={saving || reloading}
              className={input}
              onChange={(event) => {
                const next = event.target.value;
                setDraft(next);
                dirtyRef.current = baselineRef.current
                  ? next !== limitDraft(baselineRef.current.limitBytes)
                  : false;
                setFeedback(undefined);
              }}
            />
            <span className="text-[12px] text-content/45">MB</span>
            <SecondaryButton
              disabled={
                saving || reloading || conflict || !dirty || !validDraft
              }
              onClick={() => void save()}
            >
              {saving ? <Loader className="size-3.5 animate-spin" /> : null}
              {saving ? "Saving…" : "Save limit"}
            </SecondaryButton>
          </Row>
          {conflict ? (
            <div className="flex flex-wrap items-center gap-2 border-t border-content/5 px-4 py-3">
              <p role="alert" className="text-[12px] text-content/60">
                The app-wide limit changed elsewhere. Reload the saved limit
                before saving.
              </p>
              <SecondaryButton
                disabled={saving || reloading}
                onClick={() => void reloadSavedLimit()}
              >
                {reloading ? "Loading…" : "Reload saved limit"}
              </SecondaryButton>
              {feedback?.kind === "error" ? (
                <p role="alert" className="w-full text-[12px] text-red-400">
                  {feedback.text}
                </p>
              ) : null}
            </div>
          ) : !validDraft && dirty ? (
            <p role="alert" className="px-4 py-3 text-[12px] text-red-400">
              Enter a whole number from {MIN_RECOVERY_STORAGE_MIB} to{" "}
              {MAX_RECOVERY_STORAGE_MIB} MB.
            </p>
          ) : feedback ? (
            <p
              role={feedback.kind === "error" ? "alert" : "status"}
              className={`px-4 py-3 text-[12px] ${
                feedback.kind === "error" ? "text-red-400" : "text-content/50"
              }`}
            >
              {feedback.text}
            </p>
          ) : null}
          <details className="group/recovery-details border-t border-content/5 px-4 py-3">
            <summary className="flex cursor-pointer list-none items-center gap-2 text-[12px] text-content/45 hover:text-content/70 focus-visible:outline-1 focus-visible:outline-accent [&::-webkit-details-marker]:hidden">
              <ChevronRight className="size-3 shrink-0 transition-transform group-open/recovery-details:rotate-90" />
              About recovery storage
            </summary>
            <p className="mt-2 text-[12px] leading-relaxed text-content/45">
              The limit covers recovery data across all projects. Project totals
              may overlap when projects share saved contents.
            </p>
          </details>
        </>
      ) : (
        <div className="flex flex-wrap items-center gap-3 px-4 py-3.5">
          {loading ? (
            <p role="status" className="text-[12px] text-content/45">
              Checking recovery storage…
            </p>
          ) : (
            <>
              <p role="alert" className="text-[12px] text-red-400">
                {feedback?.text ?? "Could not read recovery storage."}
              </p>
              <SecondaryButton onClick={() => void load(true)}>
                Try again
              </SecondaryButton>
            </>
          )}
        </div>
      )}
    </Group>
  );
}
