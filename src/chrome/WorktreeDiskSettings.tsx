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
const button =
  "rounded-md border border-content/10 px-3 py-1.5 text-[12px] hover:bg-content/5 disabled:opacity-40";
const input =
  "mt-1 w-28 rounded-md border border-content/10 bg-content/5 px-2 py-1.5 text-[12px] disabled:opacity-40";

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
    <section
      aria-label="Managed checkout disk usage"
      className="space-y-4 border-b border-content/10 pb-5"
    >
      <div className="flex items-center justify-between gap-3">
        <h2 className="text-[13px] font-medium">Checkout disk capacity</h2>
        <button className={button} onClick={() => void load(false, true)}>
          Measure now
        </button>
      </div>
      {diskWarnings(usage).map((warning) => (
        <p key={warning} role="alert" className="text-[12px] text-amber-400">
          {warning}
        </p>
      ))}
      <p className="text-[12px] text-content/55">
        App-wide admission control and monitoring. Existing builds may keep
        growing. Ready workspaces stay usable under pressure. Configuration
        recovery storage has its own separate limit.
      </p>
      {usage ? (
        <>
          <dl className="grid grid-cols-2 gap-3 text-[12px] sm:grid-cols-4">
            <div>
              <dt className="text-content/50">Managed usage estimate</dt>
              <dd>{diskBytes(usage.usedBytes)}</dd>
            </div>
            <div>
              <dt className="text-content/50">Checkout budget</dt>
              <dd>
                {usage.settings.checkoutBudgetBytes === null
                  ? "Disabled"
                  : diskBytes(usage.settings.checkoutBudgetBytes)}
              </dd>
            </div>
            <div>
              <dt className="text-content/50">Safely reclaimable estimate</dt>
              <dd>{diskBytes(usage.reclaimableBytes)}</dd>
            </div>
            <div>
              <dt className="text-content/50">Pending reservations</dt>
              <dd>{diskBytes(usage.pendingBytes)}</dd>
            </div>
          </dl>
          <p className="text-[11px] text-content/45">
            Checkout measurement: {new Date(usage.measuredAt).toLocaleString()}.{" "}
            {usage.complete
              ? ""
              : "Incomplete measurement — preparation may be blocked."}
          </p>
          <ul className="space-y-1 text-[12px] text-content/65">
            {usage.volumes.map((volume) => (
              <li key={volume.id} title={volume.id}>
                Volume containing {volume.path}:{" "}
                <strong>{diskBytes(volume.availableBytes)} available</strong> ·
                measured {new Date(volume.measuredAt).toLocaleTimeString()}
              </li>
            ))}
          </ul>
          <details>
            <summary className="cursor-pointer text-[12px]">
              Per-worktree estimates ({usage.checkouts.length})
            </summary>
            <div className="mt-2 overflow-x-auto">
              <table className="w-full text-left text-[11px]">
                <thead>
                  <tr>
                    <th className="py-2">Managed checkout</th>
                    <th>Estimate</th>
                    <th>Reclaimable estimate</th>
                  </tr>
                </thead>
                <tbody>
                  {usage.checkouts.map((checkout) => (
                    <tr
                      key={checkout.id}
                      className="border-t border-content/10"
                    >
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
                      <td>{diskBytes(checkout.reclaimableBytes)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </details>
          {usage.reservations.length ? (
            <details>
              <summary className="cursor-pointer text-[12px]">
                Pending preparation ({usage.reservations.length})
              </summary>
              <ul className="mt-2 text-[11px]">
                {usage.reservations.map((reservation) => (
                  <li key={reservation.token}>
                    {reservation.path} ·{" "}
                    {reservation.operation === "awaitingSetup"
                      ? "Waiting for setup"
                      : reservation.operation}{" "}
                    · {diskBytes(reservation.remainingBytes)} remaining
                  </li>
                ))}
              </ul>
            </details>
          ) : null}
          <details>
            <summary className="cursor-pointer text-[12px]">
              Accounting limitations
            </summary>
            <ul className="mt-2 space-y-1 text-[11px] text-content/55">
              {usage.limitations.map((text) => (
                <li key={text}>{text}</li>
              ))}
              <li>
                Reclaimable space is an estimate. Cleanup reviews protection and
                recovery again before removal.
              </li>
            </ul>
          </details>
        </>
      ) : (
        <p role="status" className="text-[12px] text-content/50">
          {feedback
            ? "Disk measurements unavailable."
            : "Measuring managed checkouts…"}
        </p>
      )}
      {draft && baseline ? (
        <div className="space-y-3 border-t border-content/10 pt-3">
          <div className="flex flex-wrap gap-5 text-[12px]">
            <div>
              <label className="block">
                <input
                  type="checkbox"
                  checked={draft.budgetEnabled}
                  disabled={busy}
                  onChange={(e) => edit({ budgetEnabled: e.target.checked })}
                />{" "}
                Enable checkout budget
              </label>
              <label>
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
                />{" "}
                GiB
              </label>
            </div>
            <div>
              <label className="block">
                <input
                  type="checkbox"
                  checked={draft.reserveEnabled}
                  disabled={busy}
                  onChange={(e) => edit({ reserveEnabled: e.target.checked })}
                />{" "}
                Enable free-space reserve
              </label>
              <label>
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
                />{" "}
                GiB per volume
              </label>
            </div>
            <label>
              Initial preparation allowance
              <span className="block">
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
                />{" "}
                GiB
              </span>
            </label>
          </div>
          <p className="text-[11px] text-content/50">
            Preparation uses the larger of this allowance and the project's
            observed completed-checkout footprint, minus checkout bytes already
            accounted for.
          </p>
          <div className="flex items-center gap-3">
            <button
              className={button}
              disabled={!dirty || !validDraft || conflict || busy}
              onClick={() => void save()}
            >
              {busy ? "Saving…" : "Save disk settings"}
            </button>
            {conflict ? (
              <>
                <p role="alert" className="text-[12px]">
                  Disk settings changed elsewhere. Reload before saving.
                </p>
                <button
                  className={button}
                  disabled={busy}
                  onClick={() => void load(true)}
                >
                  Reload disk settings
                </button>
              </>
            ) : null}
          </div>
          {dirty && !validDraft ? (
            <p role="alert" className="text-[12px] text-red-400">
              Enter positive values up to 1,048,576 GiB. Disable the budget or
              reserve explicitly using its checkbox.
            </p>
          ) : null}
        </div>
      ) : null}
      {feedback ? (
        <p
          role={feedback === "Disk settings saved." ? "status" : "alert"}
          className="text-[12px]"
        >
          {feedback}
        </p>
      ) : null}
    </section>
  );
}
