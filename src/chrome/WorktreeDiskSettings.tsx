import { useCallback, useEffect, useRef, useState } from "react";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import {
  DISK_GIB,
  diskBytes,
  diskWarnings,
  mergeDiskSample,
  type DiskPressureSample,
  getWorktreeDisk,
  saveWorktreeDiskSettings,
  type DiskSettings,
  type DiskSnapshot,
} from "../lib/worktreeDisk";

import { Group, Row, Toggle, SecondaryButton } from "./SettingsControls";
import { ChevronRight } from "./icons";

type Draft = {
  budget: string;
  reserve: string;
  allowance: string;
  budgetEnabled: boolean;
  reserveEnabled: boolean;
};
const toDraft = (settings: DiskSettings): Draft => ({
  budget: String((settings.checkoutBudgetBytes ?? 30 * DISK_GIB) / DISK_GIB),
  reserve: String((settings.minimumFreeBytes ?? 10 * DISK_GIB) / DISK_GIB),
  allowance: String(settings.initialAllowanceBytes / DISK_GIB),
  budgetEnabled: settings.checkoutBudgetBytes !== null,
  reserveEnabled: settings.minimumFreeBytes !== null,
});
const valid = (value: string) =>
  value.trim() !== "" &&
  Number.isFinite(Number(value)) &&
  Number(value) > 0 &&
  Number(value) <= 1024 ** 2 &&
  Math.round(Number(value) * DISK_GIB) >= 1;
const input =
  "w-20 rounded-md border border-content/10 bg-transparent px-2 py-1 text-right text-[12px] tabular-nums text-content outline-none focus:border-content/20 focus-visible:ring-1 focus-visible:ring-accent disabled:opacity-40";

export function WorktreeDiskSettings() {
  const [usage, setUsage] = useState<DiskSnapshot | null>(null);
  const [baseline, setBaseline] = useState<DiskSettings | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [dirty, setDirty] = useState(false);
  const [conflict, setConflict] = useState(false);
  const [busy, setBusy] = useState(false);
  const [feedback, setFeedback] = useState<string | null>(null);
  const state = useRef({
    dirty: false,
    version: -1,
    measuredAt: -1,
    baseline: null as DiskSettings | null,
  });
  const operation = useRef(0);
  const lastUsage = useRef<DiskSnapshot | null>(null);
  const sample = useRef<DiskPressureSample | null>(null);
  const apply = useCallback((next: DiskSnapshot, adopt = false) => {
    if (sample.current) next = mergeDiskSample(next, sample.current);
    const current = state.current;
    if (
      next.settings.version < current.version ||
      (next.settings.version === current.version &&
        next.measuredAt < current.measuredAt)
    )
      return;
    current.version = next.settings.version;
    current.measuredAt = next.measuredAt;
    lastUsage.current = next;
    setUsage(next);
    if (adopt || !current.dirty || !current.baseline) {
      current.baseline = next.settings;
      current.dirty = false;
      setBaseline(next.settings);
      setDraft(toDraft(next.settings));
      setDirty(false);
      setConflict(false);
    } else if (current.baseline.version !== next.settings.version)
      setConflict(true);
  }, []);
  const load = useCallback(
    async (adopt = false, refresh = false) => {
      const request = ++operation.current;
      try {
        const next = await getWorktreeDisk(refresh);
        if (request === operation.current) {
          apply(next, adopt);
          setFeedback(null);
        }
      } catch (cause) {
        if (request === operation.current) setFeedback(String(cause));
      }
    },
    [apply],
  );
  useEffect(() => {
    let disposed = false;
    const stops: UnlistenFn[] = [];
    void load();
    const subscribe = <T,>(name: string, handler: (value: T) => void) => {
      void listen<T>(name, (event) => {
        if (!disposed) handler(event.payload);
      })
        .then((stop) => {
          if (disposed) stop();
          else stops.push(stop);
        })
        .catch(() => undefined);
    };
    subscribe<DiskSnapshot>("worktree-disk-snapshot", (next) => apply(next));
    subscribe<DiskPressureSample>("worktree-disk-pressure", (next) => {
      if (
        sample.current &&
        (next.measuredAt < sample.current.measuredAt ||
          next.settings.version < sample.current.settings.version)
      )
        return;
      sample.current = next;
      if (lastUsage.current) apply(lastUsage.current);
    });
    subscribe("worktree-disk-changed", () => void load());
    const focus = () => void load();
    window.addEventListener("focus", focus);
    return () => {
      disposed = true;
      operation.current++;
      stops.forEach((stop) => stop());
      window.removeEventListener("focus", focus);
    };
  }, [apply, load]);
  const edit = (patch: Partial<Draft>) => {
    if (!draft || !baseline) return;
    const next = { ...draft, ...patch };
    const changed = JSON.stringify(next) !== JSON.stringify(toDraft(baseline));
    state.current.dirty = changed;
    setDraft(next);
    setDirty(changed);
    setFeedback(null);
  };
  const validDraft =
    !!draft &&
    (!draft.budgetEnabled || valid(draft.budget)) &&
    (!draft.reserveEnabled || valid(draft.reserve)) &&
    valid(draft.allowance);
  const save = async () => {
    if (!draft || !baseline || !validDraft || conflict || busy || !dirty)
      return;
    setBusy(true);
    const request = ++operation.current;
    try {
      const saved = await saveWorktreeDiskSettings({
        ...baseline,
        checkoutBudgetBytes: draft.budgetEnabled
          ? Math.round(Number(draft.budget) * DISK_GIB)
          : null,
        minimumFreeBytes: draft.reserveEnabled
          ? Math.round(Number(draft.reserve) * DISK_GIB)
          : null,
        initialAllowanceBytes: Math.round(Number(draft.allowance) * DISK_GIB),
      });
      if (saved.version < state.current.version) {
        setConflict(true);
        return;
      }
      state.current.baseline = saved;
      state.current.version = saved.version;
      state.current.dirty = false;
      setBaseline(saved);
      setDraft(toDraft(saved));
      setDirty(false);
      setConflict(false);
      setUsage((current) =>
        current ? { ...current, settings: saved } : current,
      );
      if (request === operation.current) setFeedback("Disk settings saved.");
    } catch (cause) {
      if (String(cause).includes("WORKTREE_DISK_CONFLICT")) setConflict(true);
      else setFeedback(String(cause));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Group
      title="Disk usage"
      description="Storage limits for managed worktrees across all projects."
      action={
        <SecondaryButton onClick={() => void load(false, true)}>
          Measure now
        </SecondaryButton>
      }
    >
      <Row
        label="Managed worktrees"
        description={
          usage
            ? `${diskBytes(usage.reclaimableBytes)} estimated reclaimable · ${diskBytes(usage.pendingBytes)} reserved for preparation`
            : undefined
        }
      >
        {usage ? (
          <span className="text-[12px] tabular-nums text-content/70">
            {diskBytes(usage.usedBytes)} used
          </span>
        ) : (
          <span role="status" className="text-[12px] text-content/45">
            {feedback
              ? "Disk measurements unavailable."
              : "Measuring managed checkouts…"}
          </span>
        )}
      </Row>
      {diskWarnings(usage).length ? (
        <div className="space-y-1 border-b border-content/5 px-4 py-3">
          {diskWarnings(usage).map((warning) => (
            <p
              key={warning}
              role="alert"
              className="text-[12px] text-amber-500"
            >
              {warning}
            </p>
          ))}
        </div>
      ) : null}
      {draft && baseline ? (
        <>
          <Row
            label="Checkout budget"
            description="Limit the space available for new and restored worktrees."
          >
            <input
              className={input}
              aria-label="Checkout budget in GiB"
              type="number"
              min={0}
              max={1024 ** 2}
              step="any"
              value={draft.budget}
              disabled={busy || !draft.budgetEnabled}
              onChange={(e) => edit({ budget: e.target.value })}
            />
            <span className="mr-2 text-[12px] text-content/45">GiB</span>
            <Toggle
              label="Enable checkout budget"
              on={draft.budgetEnabled}
              disabled={busy}
              onChange={(budgetEnabled) => edit({ budgetEnabled })}
            />
          </Row>
          <Row
            label="Free-space reserve"
            description="Keep this much free space on each volume."
          >
            <input
              className={input}
              aria-label="Free-space reserve in GiB"
              type="number"
              min={0}
              max={1024 ** 2}
              step="any"
              value={draft.reserve}
              disabled={busy || !draft.reserveEnabled}
              onChange={(e) => edit({ reserve: e.target.value })}
            />
            <span className="mr-2 text-[12px] text-content/45">GiB</span>
            <Toggle
              label="Enable free-space reserve"
              on={draft.reserveEnabled}
              disabled={busy}
              onChange={(reserveEnabled) => edit({ reserveEnabled })}
            />
          </Row>
          <Row
            label="Preparation allowance"
            description="Space reserved while a new worktree is being set up."
          >
            <input
              className={input}
              aria-label="Initial preparation allowance in GiB"
              type="number"
              min={0}
              max={1024 ** 2}
              step="any"
              value={draft.allowance}
              disabled={busy}
              onChange={(e) => edit({ allowance: e.target.value })}
            />
            <span className="text-[12px] text-content/45">GiB</span>
          </Row>
        </>
      ) : null}
      {usage ? (
        <details className="group/disk border-b border-content/5">
          <summary className="flex cursor-pointer list-none items-center gap-2 px-4 py-3.5 text-[12px] text-content/50 hover:text-content focus-visible:outline-1 focus-visible:outline-accent [&::-webkit-details-marker]:hidden">
            <ChevronRight className="size-3 shrink-0 transition-transform group-open/disk:rotate-90" />
            Measurement details
          </summary>
          <div className="space-y-4 px-4 pb-4 text-[12px] text-content/55">
            <p>
              Checkout measurement:{" "}
              {new Date(usage.measuredAt).toLocaleString()}.{" "}
              {usage.complete
                ? ""
                : "Incomplete measurement — preparation may be blocked."}
            </p>
            <ul className="space-y-1">
              {usage.volumes.map((volume) => (
                <li key={volume.id} title={volume.id} className="break-words">
                  Volume containing {volume.path}:{" "}
                  <span className="text-content/75">
                    {diskBytes(volume.availableBytes)} available
                  </span>{" "}
                  · measured {new Date(volume.measuredAt).toLocaleTimeString()}
                </li>
              ))}
            </ul>
            <div className="overflow-x-auto">
              <table className="w-full text-left text-[11px]">
                <caption className="pb-2 text-left text-[12px] font-medium text-content/70">
                  Per-worktree estimates ({usage.checkouts.length})
                </caption>
                <thead>
                  <tr>
                    <th className="py-2 font-medium">Managed checkout</th>
                    <th className="pr-3 font-medium">Estimate</th>
                    <th className="font-medium">Reclaimable</th>
                  </tr>
                </thead>
                <tbody>
                  {usage.checkouts.map((checkout) => (
                    <tr key={checkout.id} className="border-t border-content/5">
                      <td className="max-w-80 break-words py-2 pr-3">
                        {checkout.path}
                        <span className="block text-content/45">
                          {checkout.projectCwd}
                          {checkout.missing ? " · Missing folder" : ""}
                          {checkout.limitations.length
                            ? ` · ${checkout.limitations.join("; ")}`
                            : ""}
                        </span>
                      </td>
                      <td className="whitespace-nowrap pr-3">
                        {diskBytes(checkout.estimatedBytes)}
                      </td>
                      <td className="whitespace-nowrap">
                        {diskBytes(checkout.reclaimableBytes)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            {usage.reservations.length ? (
              <div>
                <h3 className="mb-2 font-medium text-content/70">
                  Pending preparation ({usage.reservations.length})
                </h3>
                <ul className="space-y-1 text-[11px]">
                  {usage.reservations.map((reservation) => (
                    <li key={reservation.token} className="break-words">
                      {reservation.path} ·{" "}
                      {reservation.operation === "awaitingSetup"
                        ? "Waiting for setup"
                        : reservation.operation}{" "}
                      · {diskBytes(reservation.remainingBytes)} remaining
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
            <div>
              <h3 className="mb-2 font-medium text-content/70">
                About these estimates
              </h3>
              <ul className="space-y-1 text-[11px] leading-relaxed">
                {usage.limitations.map((text) => (
                  <li key={text}>{text}</li>
                ))}
                <li>
                  Reclaimable space is an estimate. Cleanup reviews protection
                  and recovery again before removal.
                </li>
                <li>
                  Existing builds may keep growing. Ready workspaces stay usable
                  when limits are reached. Configuration recovery storage has
                  its own separate limit.
                </li>
                <li>
                  Preparation reserves the larger of the allowance and the
                  project’s previous checkout size, minus space already counted.
                </li>
              </ul>
            </div>
          </div>
        </details>
      ) : null}
      {draft && baseline ? (
        <div className="flex flex-wrap items-center justify-end gap-3 px-4 py-3">
          {conflict ? (
            <>
              <p role="alert" className="mr-auto text-[12px] text-content/60">
                Disk settings changed elsewhere. Reload before saving.
              </p>
              <SecondaryButton disabled={busy} onClick={() => void load(true)}>
                Reload disk settings
              </SecondaryButton>
            </>
          ) : dirty && !validDraft ? (
            <p role="alert" className="mr-auto text-[12px] text-red-400">
              Enter positive values up to 1,048,576 GiB, or turn off the limit.
            </p>
          ) : null}
          <SecondaryButton
            disabled={!dirty || !validDraft || conflict || busy}
            onClick={() => void save()}
          >
            {busy ? "Saving…" : "Save disk settings"}
          </SecondaryButton>
        </div>
      ) : null}
      {feedback ? (
        <p
          role={feedback === "Disk settings saved." ? "status" : "alert"}
          className={`px-4 pb-3 text-[12px] ${feedback === "Disk settings saved." ? "text-content/50" : "text-red-400"}`}
        >
          {feedback}
        </p>
      ) : null}
    </Group>
  );
}
