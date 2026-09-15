import { invoke } from "@tauri-apps/api/core";

export const DISK_GIB = 1024 ** 3;
export type DiskSettings = {
  schemaVersion: 1;
  version: number;
  checkoutBudgetBytes: number | null;
  minimumFreeBytes: number | null;
  initialAllowanceBytes: number;
};
export type DiskVolume = {
  id: string;
  path: string;
  availableBytes: number;
  measuredAt: number;
};
export type DiskReservation = {
  token: string;
  path: string;
  operation: string;
  targetBytes: number;
  remainingBytes: number;
  volumeIds: string[];
  createdAt: number;
};
export type DiskSnapshot = {
  schemaVersion: 1;
  settings: DiskSettings;
  measuredAt: number;
  complete: boolean;
  usedBytes: number;
  reclaimableBytes: number;
  pendingBytes: number;
  checkouts: {
    id: string;
    path: string;
    projectCwd: string;
    estimatedBytes: number;
    accountedBytes: number;
    reclaimableBytes: number;
    missing: boolean;
    limitations: string[];
    volumeIds: string[];
  }[];
  volumes: DiskVolume[];
  reservations: DiskReservation[];
  limitations: string[];
};
export type DiskPressureSample = {
  settings: DiskSettings;
  volumes: DiskVolume[];
  measuredAt: number;
};
export type CapacityFailure = {
  code: "WORKTREE_CAPACITY";
  reason: "checkoutBudget" | "freeSpace" | "measurementUnavailable";
  operation: string;
  path: string;
  requiredBytes: number;
  volumeId: string | null;
  snapshot: DiskSnapshot;
  guidance: string[];
};

export const getWorktreeDisk = (refresh = false) =>
  invoke<DiskSnapshot>("worktree_disk_get", { refresh });
export const saveWorktreeDiskSettings = (settings: DiskSettings) =>
  invoke<DiskSettings>("worktree_disk_settings_set", { settings });

export function diskBytes(bytes: number): string {
  if (bytes > 0 && bytes < 0.01 * DISK_GIB) return "<0.01 GiB";
  return `${(Math.max(0, bytes) / DISK_GIB).toLocaleString(undefined, { maximumFractionDigits: 2 })} GiB`;
}

export function capacityMessage(failure: CapacityFailure): string {
  const reason =
    failure.reason === "checkoutBudget"
      ? "The managed checkout budget cannot cover this preparation."
      : failure.reason === "freeSpace"
        ? "Available disk space cannot cover this preparation and the free-space reserve."
        : "Checkout usage could not be measured completely.";
  return `${reason} Estimated additional space: ${diskBytes(failure.requiredBytes)}. Review cleanup or adjust disk settings in Settings → Worktrees, or explicitly reuse an existing workspace for a new task. Ready workspaces remain usable.`;
}
export function isCapacityFailure(value: unknown): value is CapacityFailure {
  return (
    !!value &&
    typeof value === "object" &&
    "code" in value &&
    value.code === "WORKTREE_CAPACITY" &&
    "snapshot" in value &&
    "path" in value
  );
}
export class WorkspaceCapacityError extends Error {
  constructor(readonly capacity: CapacityFailure) {
    super(capacityMessage(capacity));
    this.name = "WorkspaceCapacityError";
  }
}
/** Preserve the exact request. Native admission errors become readable in every
 * existing error surface and also reach the persistent app-wide explanation. */
export async function invokeWorkspace<T>(
  command: string,
  args: Record<string, unknown>,
): Promise<T> {
  try {
    const result = await invoke<T>(command, args);
    const path = typeof result === "string" ? result : args.path;
    if (
      command === "worktree_setup" &&
      typeof path === "string" &&
      typeof window !== "undefined"
    ) {
      window.dispatchEvent(
        new CustomEvent("worktree-capacity-ready", { detail: path }),
      );
    }
    return result;
  } catch (cause) {
    if (!isCapacityFailure(cause)) throw cause;
    if (typeof window !== "undefined") {
      window.dispatchEvent(
        new CustomEvent("worktree-capacity-failure", { detail: cause }),
      );
    }
    throw new WorkspaceCapacityError(cause);
  }
}

/** Keep checkout and volume measurements on their own clocks. */
export function mergeDiskSample(
  snapshot: DiskSnapshot,
  sample: DiskPressureSample,
): DiskSnapshot {
  if (sample.settings.version < snapshot.settings.version) return snapshot;
  const volumes = new Map(
    snapshot.volumes.map((volume) => [volume.id, volume]),
  );
  for (const volume of sample.volumes) {
    if (volume.measuredAt >= (volumes.get(volume.id)?.measuredAt ?? -1))
      volumes.set(volume.id, volume);
  }
  return {
    ...snapshot,
    settings: sample.settings,
    volumes: [...volumes.values()],
  };
}

/** Use the latest free-space sample independently of slower checkout scans. */
export function diskWarnings(
  snapshot: DiskSnapshot | null,
  sample?: DiskPressureSample | null,
): string[] {
  const settings =
    sample &&
    (!snapshot || sample.settings.version >= snapshot.settings.version)
      ? sample.settings
      : snapshot?.settings;
  if (!settings) return [];
  const warnings: string[] = [];
  if (snapshot && !snapshot.complete)
    warnings.push("Disk usage measurement is incomplete.");
  if (snapshot && settings.checkoutBudgetBytes !== null) {
    const total = snapshot.usedBytes + snapshot.pendingBytes;
    if (total >= settings.checkoutBudgetBytes)
      warnings.push(
        "Managed checkout usage and reservations have reached the budget.",
      );
    else if (total >= settings.checkoutBudgetBytes * 0.8)
      warnings.push(
        "Managed checkout usage and reservations are near the budget.",
      );
  }
  const volumes =
    sample &&
    (!snapshot ||
      sample.measuredAt >=
        Math.max(0, ...snapshot.volumes.map((v) => v.measuredAt)))
      ? sample.volumes
      : (snapshot?.volumes ?? []);
  for (const volume of volumes) {
    const pending =
      snapshot?.reservations
        .filter((r) => r.volumeIds.includes(volume.id))
        .reduce((sum, r) => sum + r.remainingBytes, 0) ?? 0;
    const available = Math.max(0, volume.availableBytes - pending);
    const reserve = settings.minimumFreeBytes ?? 0;
    if (available <= reserve || (reserve > 0 && available < reserve * 1.2)) {
      warnings.push(
        `Low free space on the volume containing ${volume.path}: ${diskBytes(volume.availableBytes)} available, ${diskBytes(pending)} pending${settings.minimumFreeBytes === null ? "" : `, ${diskBytes(reserve)} reserve`}.`,
      );
    }
  }
  return warnings;
}
