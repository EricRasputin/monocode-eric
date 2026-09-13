import { useEffect, useRef, useState } from "react";
import { BranchPicker } from "./BranchPicker";
import { Popover } from "./Popover";
import { Check, ChevronDown, Folder, GitBranch } from "./icons";
import type { Session, WorkspaceChoice } from "../lib/session";
import { sessionWorkCwd } from "../lib/session";
import { canChooseWorkspace } from "../lib/worktrees";
import { useWorktrees } from "../hooks/useWorktrees";
import { useProjectBranchesState } from "../hooks/useProjectBranches";

export function WorkspacePicker({
  session,
  enabled,
  onChange,
  onBranchChange,
  onClose,
}: {
  session: Session;
  enabled: boolean;
  onChange: (choice: WorkspaceChoice) => void;
  onBranchChange?: () => void;
  onClose?: () => void;
}) {
  const { overview, pending } = useWorktrees(session.cwd);
  const project = useProjectBranchesState(
    session.cwd,
    !!session.cwd && session.cwd !== "~",
  );
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const anchor = useRef<HTMLDivElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const draft = canChooseWorkspace(session);
  const choice = session.workspaceChoice;
  const newWorktree =
    draft &&
    (choice?.mode ??
      (overview?.settings.isolateByDefault === false
        ? "local"
        : "worktree")) === "worktree";
  const workCwd = sessionWorkCwd(session);
  const linked = workCwd !== session.cwd;
  const label = draft
    ? newWorktree
      ? "New worktree"
      : linked
        ? "Existing worktree"
        : "Current checkout"
    : linked
      ? "Worktree"
      : "Local checkout";
  useEffect(() => {
    if (open) search.current?.focus();
  }, [open]);
  useEffect(() => {
    setOpen(false);
    setQuery("");
  }, [session.id, enabled, draft]);
  const select = (next: WorkspaceChoice) => {
    onChange(next);
    setOpen(false);
    setQuery("");
    onClose?.();
  };
  // Plain folders retain the existing branch placeholder and don't offer Git actions.
  if (project.settled && !project.branches)
    return (
      <BranchPicker
        cwd={workCwd}
        enabled={enabled}
        onChange={onBranchChange}
        onClose={onClose}
      />
    );
  const existing =
    overview?.entries.filter(
      (entry) =>
        !entry.main &&
        `${entry.branch ?? "Detached HEAD"} ${entry.path}`
          .toLowerCase()
          .includes(query.toLowerCase()),
    ) ?? [];
  const optionClass =
    "flex w-full items-center gap-2 rounded-md px-2 py-2 text-left text-[12px] hover:bg-content/8 focus-visible:bg-content/8 focus-visible:outline-none";
  return (
    <>
      <div
        ref={anchor}
        className="relative min-w-0 shrink-0"
        data-workspace-picker
      >
        <button
          type="button"
          aria-label={`Workspace: ${label}`}
          aria-haspopup={draft ? "dialog" : undefined}
          aria-expanded={draft ? open : undefined}
          disabled={!enabled || !draft || pending}
          onClick={() => setOpen(!open)}
          onMouseDown={(event) => event.preventDefault()}
          title={draft ? "Choose where this conversation works" : workCwd}
          className="flex items-center gap-1.5 text-[12px] text-content/65 hover:text-content disabled:cursor-default disabled:hover:text-content/65"
        >
          {newWorktree || linked ? (
            <GitBranch className="size-3.5" />
          ) : (
            <Folder className="size-3.5" />
          )}
          <span>{label}</span>
          {draft && <ChevronDown className="size-3" />}
        </button>
        {open && (
          <Popover
            anchor={anchor}
            side="top"
            width={300}
            maxHeight={360}
            onDismiss={() => {
              setOpen(false);
              onClose?.();
            }}
            role="dialog"
            aria-label="Choose workspace"
            data-branch-picker
            className="overflow-y-auto p-1.5"
          >
            <button
              type="button"
              className={optionClass}
              onClick={() => select({ mode: "local" })}
            >
              <Folder className="size-4 shrink-0" />
              <span className="flex-1">
                Current checkout
                <span className="block text-[11px] text-content/45">
                  Work in your project folder
                </span>
              </span>
              {!newWorktree && !linked && <Check className="size-3.5" />}
            </button>
            <button
              type="button"
              className={optionClass}
              onClick={() =>
                select({ mode: "worktree", baseRef: choice?.baseRef })
              }
            >
              <GitBranch className="size-4 shrink-0" />
              <span className="flex-1">
                New worktree
                <span className="block text-[11px] text-content/45">
                  Create an isolated branch when you send
                </span>
              </span>
              {newWorktree && <Check className="size-3.5" />}
            </button>
            {overview?.entries.some((entry) => !entry.main) && (
              <div className="mt-1 border-t border-content/10 pt-1">
                <input
                  ref={search}
                  value={query}
                  onChange={(event) => setQuery(event.target.value)}
                  aria-label="Find existing worktree"
                  placeholder="Find existing worktree…"
                  className="my-1 w-full rounded bg-content/5 px-2 py-1.5 text-[12px] outline-none focus:ring-1 focus:ring-accent"
                />
                {existing.map((entry) => (
                  <button
                    type="button"
                    key={entry.path}
                    className={optionClass}
                    title={entry.path}
                    onClick={() => select({ mode: "local", path: entry.path })}
                  >
                    <GitBranch className="size-3.5 shrink-0" />
                    <span className="min-w-0 flex-1 truncate font-mono">
                      {entry.branch ?? "Detached HEAD"}
                    </span>
                    {entry.missing && (
                      <span className="text-[10px] text-content/45">
                        Restore on send
                      </span>
                    )}
                    {choice?.path === entry.path && (
                      <Check className="size-3.5 shrink-0" />
                    )}
                  </button>
                ))}
                {!existing.length && (
                  <p className="p-2 text-[12px] text-content/45">
                    No matching worktrees
                  </p>
                )}
              </div>
            )}
          </Popover>
        )}
      </div>
      <BranchPicker
        key={`${session.id}:${newWorktree}:${workCwd}`}
        cwd={newWorktree ? session.cwd : workCwd}
        branch={newWorktree ? choice?.baseRef : undefined}
        enabled={enabled && !session.busy}
        onSelectBase={
          newWorktree
            ? (baseRef) => onChange({ mode: "worktree", baseRef })
            : undefined
        }
        onChange={onBranchChange}
        onClose={onClose}
      />
    </>
  );
}
