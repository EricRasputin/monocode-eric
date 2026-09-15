import { useEffect, useState } from "react";
import { prettyCwd, projectKey, projectName } from "../lib/paths";
import {
  knownProjectPaths,
  loadArchivedProjects,
  looksLikeProject,
  normalizeProjectPath,
  subscribeArchivedProjects,
  type RecentProject,
} from "../lib/recents";
import {
  Check,
  ChevronRight,
  Worktree,
  Pin,
  PinOff,
  RefreshCw,
  Search,
} from "./icons";
import {
  WorktreeProjectPicker,
  type CleanupProject,
} from "./WorktreeProjectPicker";
import {
  pinWorktree,
  planWorktreeRetirement,
  worktreeProjectPath,
  type WorktreeEntry,
  type WorktreeRetirementPlan,
  type WorktreeRetirementReport,
} from "../lib/worktrees";
import { refreshWorktrees, useWorktrees } from "../hooks/useWorktrees";
import { WorktreeRetirementDialog } from "./WorktreeRetirementDialog";
import { WorktreeEnvironmentSettings } from "./WorktreeEnvironmentSettings";
import { WorktreeDiskSettings } from "./WorktreeDiskSettings";
import { WorktreeRecoveryStorage } from "./WorktreeRecoveryStorage";
import { WorktreeOutputCleanup } from "./WorktreeOutputCleanup";
import { WorktreeAutomaticRetirement } from "./WorktreeAutomaticRetirement";

const button =
  "inline-flex shrink-0 items-center justify-center gap-1.5 rounded-md border border-content/10 px-2.5 py-1.5 text-[12px] text-content/70 hover:bg-content/8 hover:text-content focus-visible:outline-1 focus-visible:outline-accent disabled:cursor-default disabled:opacity-40";
const quietButton =
  "inline-flex items-center gap-1.5 rounded-md px-2 py-1 text-[12px] text-content/40 hover:bg-content/8 hover:text-content focus-visible:outline-1 focus-visible:outline-accent disabled:opacity-40";

function loadCleanupProjects(): CleanupProject[] {
  const archived = new Set(
    loadArchivedProjects().map((project) => projectKey(project.path)),
  );
  return knownProjectPaths().map((path) => ({
    path,
    archived: archived.has(projectKey(path)),
  }));
}

/** Creation lives in the composer. This screen is only for reviewing disk cleanup. */
export function WorktreeManager({
  cwd,
  recents,
  onOpen,
}: {
  cwd: string;
  recents: RecentProject[];
  onOpen: (cwd: string, worktreeCwd: string) => Promise<void>;
}) {
  const [savedProjects, setSavedProjects] = useState(loadCleanupProjects);
  const [selectedKey, setSelectedKey] = useState(() => projectKey(cwd));
  const [focusPicker, setFocusPicker] = useState(false);
  useEffect(
    () =>
      subscribeArchivedProjects(() => setSavedProjects(loadCleanupProjects())),
    [],
  );
  const projectsByKey = new Map<string, CleanupProject>();
  for (const project of [
    ...savedProjects,
    ...recents.map(({ path }) => ({ path, archived: false })),
    { path: cwd, archived: false },
  ]) {
    if (!looksLikeProject(project.path)) continue;
    const path = normalizeProjectPath(project.path);
    projectsByKey.set(projectKey(path), { ...project, path });
  }
  const projects = [...projectsByKey.values()].sort(
    (a, b) =>
      projectName(a.path).localeCompare(projectName(b.path)) ||
      a.path.localeCompare(b.path),
  );
  const selected =
    projectsByKey.get(selectedKey) ??
    projectsByKey.get(projectKey(cwd)) ??
    projects[0];
  if (!selected) {
    return (
      <div className="space-y-6">
        <WorktreeDiskSettings />
        <p className="text-sm text-content/60">
          Open a Git project to review its worktrees. Projects you open will
          appear here.
        </p>
      </div>
    );
  }
  return (
    <div className="space-y-6">
      <WorktreeDiskSettings />
      <ProjectWorktreeCleanup
        key={projectKey(selected.path)}
        cwd={selected.path}
        projects={projects}
        focusPicker={focusPicker}
        onSelectProject={(key) => {
          setSelectedKey(key);
          setFocusPicker(true);
        }}
        onOpen={(worktreeCwd) => onOpen(selected.path, worktreeCwd)}
      />
    </div>
  );
}

/** A new project gets a fresh selection, review and status, including on return. */
function ProjectWorktreeCleanup({
  cwd,
  projects,
  onSelectProject,
  focusPicker,
  onOpen,
}: {
  cwd: string;
  projects: CleanupProject[];
  onSelectProject: (key: string) => void;
  focusPicker: boolean;
  onOpen: (worktreeCwd: string) => Promise<void>;
}) {
  const { overview, error: loadError, pending } = useWorktrees(cwd);
  const [outputReviewing, setOutputReviewing] = useState(false);
  const [selection, setSelection] = useState<string[]>([]);
  const [reviewPlan, setReviewPlan] = useState<WorktreeRetirementPlan | null>(
    null,
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const project = projectKey(overview?.projectCwd ?? cwd);
  const projectLocation = {
    repo: overview?.repo ?? cwd,
    projectCwd: overview?.projectCwd ?? cwd,
  };
  const entries = (overview?.entries ?? []).filter(
    (entry) => !entry.projectCwd || projectKey(entry.projectCwd) === project,
  );
  const ready = entries.filter(
    (entry) =>
      entry.id &&
      !entry.blockedReason &&
      (!entry.missing || entry.retirementPending),
  );
  const selected = selection.filter((id) =>
    ready.some((entry) => entry.id === id),
  );
  const matches = (entry: WorktreeEntry) =>
    `${entry.branch ?? ""} ${entry.path}`
      .toLowerCase()
      .includes(query.toLowerCase());
  const kept = entries.filter((entry) => !ready.includes(entry));
  async function run(operation: () => Promise<void>) {
    setBusy(true);
    setError(null);
    try {
      await operation();
      await refreshWorktrees(cwd);
    } catch (error) {
      setError(String(error));
    } finally {
      setBusy(false);
    }
  }
  async function reviewRetirement() {
    setBusy(true);
    setError(null);
    try {
      const plan = await planWorktreeRetirement({
        cwd,
        ids: [...selected],
      });
      if (plan.entries.length || plan.kept.length) {
        setReviewPlan(plan);
      } else {
        setError(
          "Those worktrees are no longer ready to retire. Refresh and try again.",
        );
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }
  async function retirementFinished(report: WorktreeRetirementReport) {
    const removed = new Set(
      report.results
        .filter((result) => result.worktreeRemoved)
        .map((result) => result.id),
    );
    setSelection((current) => current.filter((id) => !removed.has(id)));
    try {
      await refreshWorktrees(cwd);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }
  const displayName = (entry: WorktreeEntry) => entry.branch ?? "Detached HEAD";
  const visibleReady = ready.filter(matches);
  return (
    <section aria-label="Worktree cleanup" className="space-y-6">
      <div className="flex items-center justify-between gap-4 border-b border-content/10 pb-5">
        <WorktreeProjectPicker
          cwd={cwd}
          projects={projects}
          disabled={busy || !!reviewPlan || outputReviewing}
          autoFocus={focusPicker}
          onSelect={onSelectProject}
        />
        <button
          className={quietButton}
          disabled={busy || pending || !!reviewPlan || outputReviewing}
          onClick={() => void run(() => refreshWorktrees(cwd))}
        >
          <RefreshCw
            className={`size-3.5 ${busy || pending ? "animate-spin" : ""}`}
            strokeWidth={1.75}
          />
          Refresh
        </button>
      </div>

      {overview ? (
        <WorktreeAutomaticRetirement
          cwd={cwd}
          policy={overview.retirementPolicy}
          items={overview.automaticRetirement}
          disabled={busy || !!reviewPlan || outputReviewing}
          onSaved={() => refreshWorktrees(cwd)}
        />
      ) : null}

      {overview ? (
        <WorktreeEnvironmentSettings
          cwd={cwd}
          settings={overview.settings}
          disabled={busy || !!reviewPlan || outputReviewing}
          onSaved={() => refreshWorktrees(cwd)}
        />
      ) : null}

      <WorktreeRecoveryStorage projectCwd={overview?.projectCwd ?? cwd} />

      {overview ? (
        <WorktreeOutputCleanup
          cwd={cwd}
          entries={entries}
          disabled={busy || !!reviewPlan}
          onChanged={() => refreshWorktrees(cwd)}
          onReviewingChange={setOutputReviewing}
        />
      ) : null}

      <div>
        <h2 className="text-[13px] font-medium">Cleanup</h2>
        <p className="mt-1 text-[12px] text-content/45">
          Choose finished worktrees when you’re ready to remove them.
        </p>
      </div>

      {loadError ? (
        <div
          role="alert"
          className="rounded-lg border border-red-400/20 bg-red-400/5 p-4 text-[12px] text-red-400"
        >
          {loadError}
        </div>
      ) : pending ? (
        <div
          role="status"
          className="flex items-center gap-2 py-8 text-[12px] text-content/45"
        >
          <RefreshCw className="size-4 animate-spin" /> Checking worktrees…
        </div>
      ) : (
        <>
          <div>
            <div className="flex min-h-9 flex-wrap items-center justify-between gap-3 border-b border-content/10 pb-2">
              <h3 className="text-[12px] font-medium text-content/70">
                Ready to retire{" "}
                <span className="ml-1.5 tabular-nums text-content/35">
                  {ready.length}
                </span>
              </h3>
              {entries.length > 1 && (
                <div className="flex w-48 items-center gap-1.5">
                  <Search className="size-3.5 shrink-0 text-content/30" />
                  <input
                    aria-label="Find worktree to retire"
                    placeholder="Filter worktrees…"
                    value={query}
                    onChange={(event) => setQuery(event.target.value)}
                    className="min-w-0 flex-1 rounded bg-transparent py-1 text-[12px] outline-none placeholder:text-content/30 focus-visible:ring-1 focus-visible:ring-accent"
                  />
                </div>
              )}
            </div>
            <div className="divide-y divide-content/5">
              {visibleReady.map((entry) => (
                <div
                  key={entry.id}
                  className="group flex items-center gap-3 py-4"
                >
                  <label className="flex min-w-0 flex-1 cursor-pointer items-center gap-3">
                    <input
                      type="checkbox"
                      aria-label={`Select ${displayName(entry)}`}
                      checked={selected.includes(entry.id!)}
                      disabled={busy || !!reviewPlan || outputReviewing}
                      className="size-3.5 shrink-0 accent-accent"
                      onChange={(event) =>
                        setSelection(
                          event.target.checked
                            ? [...selected, entry.id!]
                            : selected.filter((id) => id !== entry.id),
                        )
                      }
                    />
                    <Worktree
                      className="size-4 shrink-0 text-content/40"
                      strokeWidth={1.75}
                    />
                    <span className="min-w-0">
                      <span className="block truncate text-[12px] font-medium">
                        {displayName(entry)}
                      </span>
                      <span
                        className="mt-1 block truncate text-[11px] text-content/35"
                        title={entry.path}
                      >
                        {prettyCwd(entry.path)}
                      </span>
                    </span>
                  </label>
                  <button
                    className={quietButton}
                    title="Keep this worktree"
                    disabled={busy || !!reviewPlan || outputReviewing}
                    onClick={() => void run(() => pinWorktree(entry.id!, true))}
                  >
                    <Pin className="size-3.5" /> Pin
                  </button>
                </div>
              ))}
            </div>
            {!ready.length && overview ? (
              <div className="flex flex-col items-center py-10 text-center">
                <div className="mb-3 flex size-9 items-center justify-center rounded-full bg-content/5 text-content/35">
                  <Check className="size-4" />
                </div>
                <p className="text-[13px] font-medium text-content/75">
                  Nothing ready to retire
                </p>
                <p className="mt-1 max-w-sm text-[12px] leading-relaxed text-content/40">
                  Worktrees in use, pinned, or with uncommitted changes stay
                  safely listed below.
                </p>
              </div>
            ) : !visibleReady.length ? (
              <p className="py-8 text-center text-[12px] text-content/40">
                No worktrees match “{query}”
              </p>
            ) : null}

            {ready.length > 0 && (
              <div className="flex items-center justify-between gap-3 border-t border-content/10 py-3">
                <span className="text-[11px] text-content/40">
                  {selected.length} selected
                </span>
                <button
                  className={`${button} bg-content/5`}
                  disabled={
                    busy || !!reviewPlan || outputReviewing || !selected.length
                  }
                  onClick={() => void reviewRetirement()}
                >
                  Review retirement ({selected.length}){" "}
                  <ChevronRight className="size-3" />
                </button>
              </div>
            )}
          </div>

          <details className="group/kept border-t border-content/10 pt-4">
            <summary className="flex cursor-pointer list-none items-center gap-2 text-[12px] text-content/50 [&::-webkit-details-marker]:hidden">
              <ChevronRight className="size-3 transition-transform group-open/kept:rotate-90" />
              Kept worktrees{" "}
              <span className="text-content/30">{kept.length}</span>
            </summary>
            <div className="mt-2 divide-y divide-content/5">
              {kept.filter(matches).map((entry) => (
                <div key={entry.path} className="flex items-center gap-3 py-3">
                  <Worktree
                    className="size-4 shrink-0 text-content/30"
                    strokeWidth={1.75}
                  />
                  <div className="min-w-0 flex-1">
                    <p
                      className="truncate text-[12px] text-content/70"
                      title={entry.path}
                    >
                      {displayName(entry)}
                    </p>
                    <p className="mt-1 text-[11px] text-content/35">
                      {entry.blockedReason ??
                        (entry.missing
                          ? "Working folder is missing"
                          : "Not available to retire")}
                    </p>
                  </div>
                  {entry.id && (
                    <button
                      className={quietButton}
                      disabled={busy || outputReviewing}
                      onClick={() =>
                        void run(() => pinWorktree(entry.id!, !entry.pinned))
                      }
                    >
                      {entry.pinned ? (
                        <PinOff className="size-3.5" />
                      ) : (
                        <Pin className="size-3.5" />
                      )}
                      {entry.pinned ? "Unpin" : "Pin"}
                    </button>
                  )}
                  <button
                    className={quietButton}
                    disabled={busy || outputReviewing}
                    onClick={() =>
                      void run(() =>
                        onOpen(
                          worktreeProjectPath(projectLocation, entry.path),
                        ),
                      )
                    }
                  >
                    {entry.missing ? "Restore" : "Open"}
                  </button>
                </div>
              ))}
              {query && !kept.some(matches) && (
                <p className="py-4 text-[12px] text-content/40">
                  No kept worktrees match “{query}”
                </p>
              )}
            </div>
          </details>
        </>
      )}
      {busy && (
        <p role="status" className="text-[12px] text-content/45">
          Checking…
        </p>
      )}
      {error && (
        <p role="alert" className="text-[12px] text-red-400">
          {error}
        </p>
      )}
      {reviewPlan ? (
        <WorktreeRetirementDialog
          plan={reviewPlan}
          source="settings"
          onClose={() => setReviewPlan(null)}
          onRetired={(report) => void retirementFinished(report)}
        />
      ) : null}
    </section>
  );
}
