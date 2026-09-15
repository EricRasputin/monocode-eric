import {
  createContext,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from "react";

export const WorkspacePreparation = createContext<
  ((cwd: string, path?: string) => Promise<string>) | null
>(null);

/** Mount filesystem users only after native preparation, including restored
 * editor/terminal panes. A mounted surface retains its lease; each new workspace
 * action still checks native setup through the shared preparation interface. */
export function WorkspaceAccess({
  cwd,
  path,
  children,
}: {
  cwd: string;
  path?: string;
  children: (cwd: string) => ReactNode;
}) {
  const prepare = useContext(WorkspacePreparation);
  const key = `${cwd}\0${path ?? ""}`;
  const [state, setState] = useState<{
    key: string;
    cwd?: string;
    error?: string;
  }>();
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    if (!prepare) return;
    let cancelled = false;
    setState(undefined);
    void prepare(cwd, path).then(
      (prepared) => {
        if (!cancelled) setState({ key, cwd: prepared });
      },
      (error) => {
        if (!cancelled) setState({ key, error: String(error) });
      },
    );
    return () => {
      cancelled = true;
    };
  }, [prepare, cwd, path, key, attempt]);
  if (!prepare) return children(cwd);
  if (state?.key === key && state.cwd) return children(state.cwd);
  return (
    <div className="p-4 text-xs text-content/60" role="status">
      {state?.key === key && state.error ? (
        <>
          <p>{state.error}</p>
          <button
            className="mt-2 text-accent"
            onClick={() => setAttempt((value) => value + 1)}
          >
            Retry workspace preparation
          </button>
        </>
      ) : (
        "Preparing workspace…"
      )}
    </div>
  );
}
