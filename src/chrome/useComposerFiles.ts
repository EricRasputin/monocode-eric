import { useEffect, useState } from "react";
import {
  loadProjectFiles,
  peekProjectFiles,
  subscribeProjectFiles,
} from "../lib/fileIndex";
import type { ProjectFile } from "../lib/fs";

/** File mentions belong to the execution checkout, including cache updates. */
export function useComposerFiles(
  cwd: string,
  enabled: boolean,
  refresh: boolean,
): ProjectFile[] {
  const [state, setState] = useState(() => ({
    cwd,
    files: enabled ? (peekProjectFiles(cwd) ?? []) : [],
  }));
  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const apply = (files: ProjectFile[]) => {
      if (!cancelled) setState({ cwd, files });
    };
    apply(peekProjectFiles(cwd) ?? []);
    void loadProjectFiles(cwd, refresh)
      .then(apply)
      .catch(() => undefined);
    const unsubscribe = subscribeProjectFiles(() => {
      const files = peekProjectFiles(cwd);
      if (files) apply(files);
    });
    return () => {
      cancelled = true;
      unsubscribe();
    };
  }, [cwd, enabled, refresh]);
  return enabled
    ? state.cwd === cwd
      ? state.files
      : (peekProjectFiles(cwd) ?? [])
    : [];
}
