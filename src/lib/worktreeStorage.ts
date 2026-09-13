import { invoke } from "@tauri-apps/api/core";

export const RECOVERY_STORAGE_MIB = 1024 * 1024;
export const MIN_RECOVERY_STORAGE_MIB = 1;
export const MAX_RECOVERY_STORAGE_MIB = 4096;

export type RecoveryStorageUsage = {
  usedBytes: number;
  limitBytes: number;
  version: number;
  projects: { projectCwd: string; usedBytes: number }[];
};

export type RecoveryStoragePressure = "normal" | "warning" | "full";

export function recoveryStoragePressure(
  usedBytes: number,
  limitBytes: number,
): RecoveryStoragePressure {
  if (limitBytes <= 0 || usedBytes >= limitBytes) return "full";
  if (usedBytes / limitBytes >= 0.8) return "warning";
  return "normal";
}

export function formatRecoveryStorageBytes(bytes: number): string {
  const megabytes = Math.max(0, bytes) / RECOVERY_STORAGE_MIB;
  const rounded = Math.round(megabytes * 10) / 10;
  return `${Number.isInteger(rounded) ? rounded.toFixed(0) : rounded.toFixed(1)} MB`;
}

export const getRecoveryStorage = () =>
  invoke<RecoveryStorageUsage>("worktree_storage_get");

export const setRecoveryStorageLimit = (
  limitBytes: number,
  expectedVersion: number,
) =>
  invoke<RecoveryStorageUsage>("worktree_storage_limit_set", {
    limitBytes,
    expectedVersion,
  });
