import { useEffect, useRef, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  capacityMessage,
  diskWarnings,
  getWorktreeDisk,
  type CapacityFailure,
  type DiskPressureSample,
  type DiskSnapshot,
} from "../lib/worktreeDisk";

export function WorktreeDiskPressure({
  onOpenSettings,
}: {
  onOpenSettings: () => void;
}) {
  const [snapshot, setSnapshot] = useState<DiskSnapshot | null>(null);
  const [sample, setSample] = useState<DiskPressureSample | null>(null);
  const [failure, setFailure] = useState<CapacityFailure | null>(null);
  const [error, setError] = useState<string | null>(null);
  const latest = useRef({ snapshot: -1, sample: -1, version: -1 });
  useEffect(() => {
    let disposed = false;
    const stops: UnlistenFn[] = [];
    const apply = (next: DiskSnapshot) => {
      if (
        disposed ||
        next.measuredAt < latest.current.snapshot ||
        next.settings.version < latest.current.version
      )
        return;
      latest.current.snapshot = next.measuredAt;
      latest.current.version = next.settings.version;
      setSnapshot(next);
      setError(null);
    };
    const applySample = (next: DiskPressureSample) => {
      if (
        disposed ||
        next.measuredAt < latest.current.sample ||
        next.settings.version < latest.current.version
      )
        return;
      latest.current.sample = next.measuredAt;
      latest.current.version = next.settings.version;
      setSample(next);
    };
    const load = () =>
      void getWorktreeDisk()
        .then(apply)
        .catch((cause) => {
          if (!disposed) setError(String(cause));
        });
    const subscribe = <T,>(name: string, handler: (value: T) => void) => {
      void listen<T>(name, (event) => handler(event.payload))
        .then((stop) => {
          if (disposed) stop();
          else stops.push(stop);
        })
        .catch(() => undefined);
    };
    subscribe<DiskSnapshot>("worktree-disk-snapshot", apply);
    subscribe<DiskPressureSample>("worktree-disk-pressure", applySample);
    subscribe<string>("worktree-disk-monitor-error", (value) =>
      setError(value),
    );
    subscribe("worktree-disk-changed", load);
    const onFailure = (event: Event) => {
      const value = (event as CustomEvent<CapacityFailure>).detail;
      setFailure(value);
      apply(value.snapshot);
    };
    const onReady = (event: Event) => {
      const path = (event as CustomEvent<string>).detail;
      setFailure((current) =>
        current &&
        (path === current.path || path.startsWith(`${current.path}/`))
          ? null
          : current,
      );
    };
    window.addEventListener("worktree-capacity-failure", onFailure);
    window.addEventListener("worktree-capacity-ready", onReady);
    window.addEventListener("focus", load);
    load();
    return () => {
      disposed = true;
      stops.forEach((stop) => stop());
      window.removeEventListener("worktree-capacity-failure", onFailure);
      window.removeEventListener("worktree-capacity-ready", onReady);
      window.removeEventListener("focus", load);
    };
  }, []);
  const warnings = diskWarnings(snapshot, sample);
  if (!failure && !warnings.length && !error) return null;
  return (
    <aside
      role="alert"
      aria-label="Workspace disk capacity"
      className="shrink-0 border-b border-amber-400/25 bg-amber-400/10 px-4 py-2 text-[12px] text-content/80"
    >
      <p className="font-medium">
        {failure
          ? "Workspace preparation needs disk capacity"
          : "Disk pressure"}
      </p>
      {failure ? (
        <p>
          {capacityMessage(failure)} Requested workspace: {failure.path}
        </p>
      ) : null}
      {warnings.map((warning) => (
        <p key={warning}>{warning}</p>
      ))}
      {error ? (
        <p>
          Disk monitoring is unavailable: {error}. Last known measurements may
          be stale.
        </p>
      ) : null}
      <p>Existing builds may keep growing. Ready workspaces remain usable.</p>
      <div className="mt-1 flex flex-wrap items-center gap-3">
        <button
          className="underline focus-visible:outline-1"
          onClick={onOpenSettings}
        >
          Review cleanup
        </button>
        <button
          className="underline focus-visible:outline-1"
          onClick={onOpenSettings}
        >
          Disk settings
        </button>
        <span>
          For a new task, explicitly choose an existing workspace to reuse.
        </span>
      </div>
    </aside>
  );
}
