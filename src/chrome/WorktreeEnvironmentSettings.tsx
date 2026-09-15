import { useEffect, useState } from "react";
import {
  listWorktrees,
  saveWorktreeSettings,
  type WorktreeSettings,
} from "../lib/worktrees";
import { Loader } from "./icons";
import { Group, Row, SecondaryButton } from "./SettingsControls";

type EnvironmentDraft = {
  setupCommand: string;
  copyPaths: string;
  disposablePaths: string;
};

const field =
  "block w-72 max-w-full resize-y rounded-md border border-content/10 bg-transparent px-2.5 py-2 font-mono text-[12px] leading-5 text-content/75 outline-none placeholder:text-content/25 focus:border-accent/60 focus:ring-1 focus:ring-accent/30 disabled:cursor-default disabled:opacity-50";

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
    <Group
      title="Environment"
      description="Prepare new and restored worktrees for this project."
    >
      <Row
        label="Setup command"
        description="Run from this project’s folder after local files are copied."
      >
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
      </Row>
      <Row
        label="Copy local files"
        description="Ignored files to copy from the main checkout and preserve when retiring. One project-relative path per line."
      >
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
      </Row>
      <Row
        label="Disposable folders"
        description="Extra generated folders to allow for cleanup, one path per line. Standard build folders are already recognized; copied files are preserved."
      >
        <textarea
          aria-label="Disposable folders"
          rows={2}
          spellCheck={false}
          disabled={busy}
          value={draft.disposablePaths}
          placeholder={"coverage\n.next"}
          className={field}
          onChange={(event) => {
            setDraft((current) => ({
              ...current,
              disposablePaths: event.target.value,
            }));
            setFeedback(undefined);
          }}
        />
      </Row>
      <div className="flex flex-wrap items-center justify-between gap-3 px-4 py-3.5">
        <div className="min-w-0 flex-1">
          {conflict ? (
            <div className="flex flex-wrap items-center gap-2">
              <p role="alert" className="text-[12px] text-content/60">
                Settings changed in another window. Reload them before saving.
              </p>
              <SecondaryButton disabled={busy} onClick={() => void reload()}>
                {reloading ? "Loading…" : "Reload saved settings"}
              </SecondaryButton>
              {feedback?.kind === "error" ? (
                <p role="alert" className="w-full text-[12px] text-red-400">
                  {feedback.text}
                </p>
              ) : null}
            </div>
          ) : feedback ? (
            <p
              role={feedback.kind === "error" ? "alert" : "status"}
              className={`text-[12px] ${feedback.kind === "error" ? "text-red-400" : "text-content/45"}`}
            >
              {feedback.text}
            </p>
          ) : (
            <p className="text-[12px] text-content/45">
              Applies to this project only.
            </p>
          )}
        </div>
        <SecondaryButton
          disabled={busy || conflict}
          onClick={() => void save()}
        >
          {saving ? <Loader className="size-3.5 animate-spin" /> : null}
          {saving ? "Saving…" : "Save environment"}
        </SecondaryButton>
      </div>
    </Group>
  );
}
