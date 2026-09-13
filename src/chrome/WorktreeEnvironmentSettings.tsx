import { useEffect, useState } from "react";
import {
  listWorktrees,
  saveWorktreeSettings,
  type WorktreeSettings,
} from "../lib/worktrees";
import { ChevronRight, Loader } from "./icons";

type EnvironmentDraft = {
  setupCommand: string;
  copyPaths: string;
  disposablePaths: string;
};

const field =
  "mt-1.5 w-full resize-y rounded-md border border-content/10 bg-content/3 px-2.5 py-2 font-mono text-[11px] leading-4 text-content/75 outline-none placeholder:text-content/25 focus:border-accent/60 focus:ring-1 focus:ring-accent/30 disabled:cursor-default disabled:opacity-50";
const saveButton =
  "inline-flex items-center justify-center gap-1.5 rounded-md border border-content/10 bg-content/5 px-2.5 py-1.5 text-[12px] font-medium text-content/70 hover:bg-content/8 hover:text-content focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:cursor-default disabled:opacity-40";

function pathsFromText(value: string): string[] {
  return value
    .split("\n")
    .map((path) => path.trim())
    .filter(Boolean);
}

function draftFromSettings(settings: WorktreeSettings): EnvironmentDraft {
  return {
    setupCommand: settings.setupCommand ?? "",
    copyPaths: (settings.copyPaths ?? []).join("\n"),
    disposablePaths: (settings.disposablePaths ?? []).join("\n"),
  };
}

export function WorktreeEnvironmentSettings({
  cwd,
  settings,
  disabled = false,
  onSaved,
}: {
  cwd: string;
  settings: WorktreeSettings;
  disabled?: boolean;
  onSaved?: () => Promise<void> | void;
}) {
  const [draft, setDraft] = useState<EnvironmentDraft>(() =>
    draftFromSettings(settings),
  );
  const [baseline, setBaseline] = useState(settings);
  const [saving, setSaving] = useState(false);
  const [reloading, setReloading] = useState(false);
  const [saveConflict, setSaveConflict] = useState(false);
  const [feedback, setFeedback] = useState<
    { kind: "saved" | "error"; text: string } | undefined
  >();
  const dirty =
    JSON.stringify(draft) !== JSON.stringify(draftFromSettings(baseline));
  const newerSettings =
    (settings.environmentVersion ?? 0) > (baseline.environmentVersion ?? 0);
  const conflict = saveConflict || (dirty && newerSettings);
  const busy = disabled || saving || reloading;

  useEffect(() => {
    if (!dirty && !saving && !reloading && newerSettings) {
      setBaseline(settings);
      setDraft(draftFromSettings(settings));
      setFeedback(undefined);
      setSaveConflict(false);
    }
  }, [settings, dirty, saving, reloading, newerSettings]);

  const reload = async () => {
    setReloading(true);
    setFeedback(undefined);
    try {
      const latest = await listWorktrees(cwd);
      setBaseline(latest.settings);
      setDraft(draftFromSettings(latest.settings));
      setSaveConflict(false);
      setFeedback(undefined);
      await onSaved?.();
    } catch (cause) {
      setFeedback({
        kind: "error",
        text: cause instanceof Error ? cause.message : String(cause),
      });
    } finally {
      setReloading(false);
    }
  };

  const save = async () => {
    if (busy || conflict) return;
    setSaving(true);
    setFeedback(undefined);
    try {
      const saved = await saveWorktreeSettings(cwd, {
        ...baseline,
        environmentVersion: baseline.environmentVersion ?? 0,
        setupCommand: draft.setupCommand.trim(),
        copyPaths: pathsFromText(draft.copyPaths),
        disposablePaths: pathsFromText(draft.disposablePaths),
      });
      setBaseline(saved);
      setDraft(draftFromSettings(saved));
      setSaveConflict(false);
      setFeedback({
        kind: "saved",
        text: "Saved. New and restored worktrees will use this environment.",
      });
      await onSaved?.();
    } catch (cause) {
      const text = cause instanceof Error ? cause.message : String(cause);
      if (text.includes("WORKTREE_SETTINGS_CONFLICT")) {
        setSaveConflict(true);
        return;
      }
      setFeedback({
        kind: "error",
        text,
      });
    } finally {
      setSaving(false);
    }
  };

  return (
    <details className="group/environment border-b border-content/10 pb-5">
      <summary className="flex cursor-pointer list-none items-center gap-2 rounded-md py-1 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent [&::-webkit-details-marker]:hidden">
        <ChevronRight className="size-3 shrink-0 text-content/35 transition-transform group-open/environment:rotate-90" />
        <span>
          <span className="block text-[12px] font-medium text-content/70">
            Worktree environment
          </span>
          <span className="mt-0.5 block text-[11px] text-content/35">
            Copy local config and run setup when a worktree is prepared.
          </span>
        </span>
      </summary>

      <div className="ml-5 mt-4 space-y-4">
        <label className="block text-[12px] text-content/65">
          Setup command
          <textarea
            aria-label="Setup command"
            rows={2}
            spellCheck={false}
            disabled={busy}
            value={draft.setupCommand}
            placeholder="npm ci"
            className={field}
            onChange={(event) => {
              setDraft((current) => ({
                ...current,
                setupCommand: event.target.value,
              }));
              setFeedback(undefined);
            }}
          />
          <span className="mt-1 block text-[11px] leading-4 text-content/35">
            Runs from this project’s folder in the worktree after local files
            are copied.
          </span>
        </label>

        <label className="block text-[12px] text-content/65">
          Copy local files
          <textarea
            aria-label="Copy local files"
            rows={2}
            spellCheck={false}
            disabled={busy}
            value={draft.copyPaths}
            placeholder={".env.local\n.config/project.json"}
            className={field}
            onChange={(event) => {
              setDraft((current) => ({
                ...current,
                copyPaths: event.target.value,
              }));
              setFeedback(undefined);
            }}
          />
          <span className="mt-1 block text-[11px] leading-4 text-content/35">
            Paths relative to this project, one ignored file per line. Copied
            from the main checkout and preserved when retiring.
          </span>
        </label>

        <label className="block text-[12px] text-content/65">
          Disposable folders
          <textarea
            aria-label="Disposable folders"
            rows={2}
            spellCheck={false}
            disabled={busy}
            value={draft.disposablePaths}
            placeholder={"node_modules\n.next"}
            className={field}
            onChange={(event) => {
              setDraft((current) => ({
                ...current,
                disposablePaths: event.target.value,
              }));
              setFeedback(undefined);
            }}
          />
          <span className="mt-1 block text-[11px] leading-4 text-content/35">
            Deleted with the worktree and recreated by setup. One ignored folder
            per line, relative to this project.
          </span>
        </label>

        <div className="flex min-h-8 flex-wrap items-center justify-between gap-3 border-t border-content/8 pt-3">
          {conflict ? (
            <div className="flex flex-wrap items-center gap-2">
              <p role="alert" className="text-[11px] text-content/60">
                Settings changed in another window. Reload them before saving.
              </p>
              <button
                type="button"
                className={saveButton}
                disabled={busy}
                onClick={() => void reload()}
              >
                {reloading ? "Loading…" : "Reload saved settings"}
              </button>
              {feedback?.kind === "error" ? (
                <p role="alert" className="w-full text-[11px] text-red-400">
                  {feedback.text}
                </p>
              ) : null}
            </div>
          ) : feedback ? (
            <p
              role={feedback.kind === "error" ? "alert" : "status"}
              className={`text-[11px] ${feedback.kind === "error" ? "text-red-400" : "text-content/50"}`}
            >
              {feedback.text}
            </p>
          ) : (
            <span className="text-[11px] text-content/30">
              Applies to this project only.
            </span>
          )}
          <button
            type="button"
            className={saveButton}
            disabled={busy || conflict}
            onClick={() => void save()}
          >
            {saving ? <Loader className="size-3.5 animate-spin" /> : null}
            {saving ? "Saving…" : "Save environment"}
          </button>
        </div>
      </div>
    </details>
  );
}
