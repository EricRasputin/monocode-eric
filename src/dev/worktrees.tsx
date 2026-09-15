import { mockIPC, mockWindows } from "@tauri-apps/api/mocks";
import { emit } from "@tauri-apps/api/event";
import type {
  WorktreeEntry,
  WorktreeOverview,
  WorktreeRetirementPlan,
  WorktreeRetirementReport,
  WorktreeRetirementSelection,
  WorktreeSettings,
  WorktreeRetirementPolicy,
  AutomaticRetirement,
} from "../lib/worktrees";
import {
  DISK_GIB,
  type DiskSettings,
  type DiskSnapshot,
} from "../lib/worktreeDisk";
import {
  RECOVERY_STORAGE_MIB,
  type RecoveryStorageUsage,
} from "../lib/worktreeStorage";
import "../index.css";

// A separate Vite entry: never imported by the desktop app or its release bundle.
if (!import.meta.env.DEV) throw new Error("This preview is development-only.");

const paths = [
  "monocode-eric",
  "Monefy",
  "proton scribe",
  "realm-walker-wiki",
].map((name) => `/Users/demo/Projects/${name}`);
const recents = paths.map((path) => ({ path, openedAt: Date.now() }));
type Scenario = "Populated" | "Empty" | "Loading" | "Error" | "Restoration";
type ArchiveDemo = "shared" | "final" | "bulk" | "blocker" | "partial";
const previewParams = new URLSearchParams(location.search);
const requestedScenario = previewParams.get("state");
const scenario: Scenario =
  requestedScenario === "Empty" ||
  requestedScenario === "Loading" ||
  requestedScenario === "Error" ||
  requestedScenario === "Restoration"
    ? requestedScenario
    : "Populated";
const day = 86_400_000;
const entries = new Map<string, WorktreeEntry[]>();
const projectSettings = new Map<string, WorktreeSettings>();
const retirementPolicies = new Map<string, WorktreeRetirementPolicy>();
const automaticJobs = new Map<string, AutomaticRetirement[]>();
const currentPolicy = (cwd: string): WorktreeRetirementPolicy =>
  retirementPolicies.get(cwd) ?? {
    schemaVersion: 1,
    version: 0,
    mode: "manual",
  };
// Explicit preview scenario: represents a preference the fixture user saved.
if (previewParams.get("automatic") === "1") {
  retirementPolicies.set(paths[0], {
    schemaVersion: 1,
    version: 1,
    mode: "automatic",
  });
  automaticJobs.set(paths[0], [
    {
      id: "preview-pinned",
      path: `${paths[0]}/.worktrees/pinned`,
      planId: null,
      status: "blocked",
      reason: "Conversation is pinned",
      updatedAt: Date.now(),
    },
    {
      id: "preview-partial",
      path: `${paths[0]}/.worktrees/partial`,
      planId: "retirement-pending",
      status: "failed",
      reason:
        "Checkout removed; final recovery journal write will retry. Branches are kept.",
      updatedAt: Date.now(),
    },
  ]);
}
let diskPolicy: DiskSettings = {
  schemaVersion: 1,
  version: 0,
  checkoutBudgetBytes: 30 * DISK_GIB,
  minimumFreeBytes: 10 * DISK_GIB,
  initialAllowanceBytes: 5 * DISK_GIB,
};
function diskSnapshot(): DiskSnapshot {
  return {
    schemaVersion: 1,
    settings: diskPolicy,
    measuredAt: Date.now(),
    complete: true,
    usedBytes: 12 * DISK_GIB,
    reclaimableBytes: 4 * DISK_GIB,
    pendingBytes: 0,
    checkouts: [],
    reservations: [],
    volumes: [
      {
        id: "preview-volume",
        path: "/Users/demo",
        availableBytes: 42 * DISK_GIB,
        measuredAt: Date.now(),
      },
    ],
    limitations: ["Preview measurements use sample data."],
  };
}
function currentSettings(cwd: string): WorktreeSettings {
  return (
    projectSettings.get(cwd) ?? {
      isolateByDefault: true,
    }
  );
}
const retirementPlans = new Map<string, WorktreeRetirementPlan>();
let archiveDemo: ArchiveDemo = "final";
let partialAttempt = 0;
let restorationComplete = false;
let previewSetupFailure = false;
let previewArchived = true;
const workspaceCalls: string[] = [];
let recoveryStorageVersion = 2;
let recoveryStorageLimit = 64 * RECOVERY_STORAGE_MIB;
let recoveryStorageUsed = 51.2 * RECOVERY_STORAGE_MIB;
const recoveryStorageProjects = new Map(
  paths.map((path, index) => [
    path,
    (index === 0 ? 22.6 : 4 + index * 3) * RECOVERY_STORAGE_MIB,
  ]),
);

function recoveryStorage(): RecoveryStorageUsage {
  return {
    usedBytes: Math.round(recoveryStorageUsed),
    limitBytes: recoveryStorageLimit,
    version: recoveryStorageVersion,
    projects: [...recoveryStorageProjects].map(([projectCwd, usedBytes]) => ({
      projectCwd,
      usedBytes: Math.round(usedBytes),
    })),
  };
}

function addRecoveryStorage(projectCwd: string, bytes: number) {
  recoveryStorageUsed += bytes;
  recoveryStorageProjects.set(
    projectCwd,
    (recoveryStorageProjects.get(projectCwd) ?? 0) + bytes,
  );
}

function seed(cwd: string): WorktreeEntry[] {
  const entry = (
    id: string,
    branch: string,
    age: number,
    blockedReason: string | null = null,
  ): WorktreeEntry => ({
    id: `${cwd}:${id}`,
    path: `${cwd}/.worktrees/${id}`,
    branch,
    baseRef: "main",
    main: false,
    pinned: false,
    missing: false,
    lastUsed: Date.now() - age * day,
    blockedReason,
  });
  return [
    {
      ...entry("main", "main", 0, "Primary checkout"),
      id: null,
      path: cwd,
      main: true,
    },
    entry("settings", "monocode/settings-polish", 12),
    entry("keyboard", "monocode/keyboard-navigation", 9),
    entry(
      "composer",
      "monocode/composer-layout",
      1,
      "Used by an open conversation",
    ),
    { ...entry("docs", "monocode/worktree-guide", 4, "Pinned"), pinned: true },
    entry(
      "api",
      "monocode/provider-retries",
      5,
      "Contains uncommitted changes",
    ),
  ];
}

function currentEntries(cwd: string) {
  if (!entries.has(cwd)) entries.set(cwd, seed(cwd));
  return entries.get(cwd)!;
}

function retirementEntry(
  id: string,
  cwd: string,
  branch: string,
  worktreeRemoved = false,
): WorktreeRetirementPlan["entries"][number] {
  return {
    id,
    repo: cwd,
    path: `${cwd}/.worktrees/${id.split(":").join("-")}`,
    branch,
    worktreeRemoved,
    blockedReason: null,
    localBranch: { name: branch, allowed: true, reason: null },
    remoteBranch: {
      name: branch,
      remote: "origin",
      destination: `github.com/demo/monocode.git · ${branch}`,
      allowed: true,
      reason: null,
    },
  };
}

const retirementResults = new Map<
  string,
  WorktreeRetirementReport["results"][number]
>();

function rememberPlan(plan: WorktreeRetirementPlan) {
  retirementPlans.set(plan.planId, plan);
  for (const entry of plan.entries)
    retirementResults.delete(`${plan.planId}:${entry.id}`);
  return plan;
}

function settingsPlan(cwd: string, ids: string[]): WorktreeRetirementPlan {
  const byId = new Map(
    currentEntries(cwd)
      .filter((entry): entry is WorktreeEntry & { id: string } => !!entry.id)
      .map((entry) => [entry.id, entry]),
  );
  const planned = ids.flatMap((id) => {
    const entry = byId.get(id);
    return entry
      ? [
          retirementEntry(
            id,
            cwd,
            entry.branch ?? "Detached work",
            !!entry.retirementPending,
          ),
        ]
      : [];
  });
  return rememberPlan({
    planId: `settings:${Date.now()}`,
    entries: planned,
    kept: ids
      .filter((id) => !byId.has(id))
      .map((id) => ({ id, path: id, reason: "No longer available" })),
  });
}

function archivePlan(): WorktreeRetirementPlan {
  const cwd = paths[0];
  if (archiveDemo === "shared") {
    return rememberPlan({ planId: "archive:shared", entries: [], kept: [] });
  }
  if (archiveDemo === "blocker") {
    return rememberPlan({
      planId: "archive:blocker",
      entries: [],
      kept: [
        {
          id: "archive-blocked",
          path: `${cwd}/.worktrees/in-progress`,
          reason: "Contains uncommitted changes",
        },
      ],
    });
  }
  const ids =
    archiveDemo === "bulk" ? ["archive-one", "archive-two"] : ["archive-final"];
  return rememberPlan({
    planId: `archive:${archiveDemo}`,
    entries: ids.map((id) =>
      retirementEntry(id, cwd, `monocode/${id.replace("archive-", "")}`),
    ),
    kept: [],
  });
}

function retirementReport(
  planId: string,
  selections: WorktreeRetirementSelection[],
): WorktreeRetirementReport {
  const plan = retirementPlans.get(planId);
  if (!plan) throw new Error("This review expired. Start a new review.");
  let storageChanged = false;
  const results = selections.map((selection) => {
    const entry = plan.entries.find(
      (candidate) => candidate.id === selection.id,
    );
    if (!entry)
      throw new Error("A selected worktree is no longer in this review.");
    const remoteFails =
      archiveDemo === "partial" &&
      partialAttempt === 0 &&
      selection.deleteRemoteBranch;
    const key = `${planId}:${selection.id}`;
    const previous = retirementResults.get(key);
    const result = {
      id: selection.id,
      path: entry.path,
      worktreeRemoved: true,
      localBranchDeleted:
        !!previous?.localBranchDeleted || selection.deleteLocalBranch,
      remoteBranchDeleted:
        !!previous?.remoteBranchDeleted ||
        (selection.deleteRemoteBranch && !remoteFails),
      recoveryRef: `monocode/recovery/${selection.id.split(":").join("-")}`,
      error: remoteFails
        ? "The working folder was removed, but the remote server could not be reached."
        : null,
    };
    retirementResults.set(key, result);
    if (!previous?.worktreeRemoved) {
      addRecoveryStorage(entry.repo, 2.4 * RECOVERY_STORAGE_MIB);
      storageChanged = true;
    }
    for (const [cwd, list] of entries) {
      const current = list.find((candidate) => candidate.id === selection.id);
      if (!current) continue;
      if (remoteFails) {
        current.missing = true;
        current.retirementPending = true;
      } else {
        entries.set(
          cwd,
          list.filter((candidate) => candidate.id !== selection.id),
        );
      }
    }
    return result;
  });
  if (storageChanged) void emit("worktree-storage-changed");
  if (archiveDemo === "partial") partialAttempt += 1;
  return { results };
}

mockWindows("main");
mockIPC(
  async (command, args) => {
    const payload = args as {
      cwd?: string;
      request?: { path?: string; cwd: string };
      archived?: boolean;
      id?: string;
      ids?: string[];
      pinned?: boolean;
      sessionIds?: string[];
      planId?: string;
      selections?: WorktreeRetirementSelection[];
      settings?: WorktreeSettings;
      policy?: WorktreeRetirementPolicy;
      limitBytes?: number;
      expectedVersion?: number;
    };
    const cwd = payload?.cwd ?? paths[0];
    if (command === "worktree_disk_get") return diskSnapshot();
    if (command === "worktree_disk_settings_set") {
      const requested = (args as { settings: DiskSettings }).settings;
      if (requested.version !== diskPolicy.version)
        throw new Error("WORKTREE_DISK_CONFLICT: Reload disk settings");
      diskPolicy = { ...requested, version: requested.version + 1 };
      return diskPolicy;
    }
    if (command === "worktree_retirement_policy_set") {
      if (payload.policy?.version !== currentPolicy(cwd).version)
        throw new Error(
          "WORKTREE_RETIREMENT_CONFLICT: Retirement preference changed in another window.",
        );
      const saved = {
        ...payload.policy!,
        version: payload.policy!.version + 1,
      };
      retirementPolicies.set(cwd, saved);
      if (saved.mode === "manual")
        automaticJobs.set(
          cwd,
          (automaticJobs.get(cwd) ?? []).map((item) =>
            item.status === "complete"
              ? item
              : {
                  ...item,
                  status: "paused",
                  reason:
                    item.reason ??
                    "Manual review is selected. Automatic retirement is paused.",
                },
          ),
        );
      void emit("worktree-retirement-changed");
      return saved;
    }
    if (command === "worktree_retirement_maintain") {
      for (const [project, jobs] of automaticJobs) {
        if (currentPolicy(project).mode !== "automatic") continue;
        automaticJobs.set(
          project,
          jobs.map((item) =>
            item.status === "blocked"
              ? item
              : {
                  ...item,
                  status: "complete",
                  reason: null,
                  updatedAt: Date.now(),
                },
          ),
        );
      }
      void emit("worktree-retirement-changed");
      return;
    }
    if (command === "worktree_archive_retirement") {
      const plan = archivePlan();
      if (currentPolicy(cwd).mode === "manual")
        return { review: plan, automatic: [] };
      const automatic: AutomaticRetirement[] = [
        ...plan.kept.map((item): AutomaticRetirement => ({
          ...item,
          planId: null,
          status: "blocked",
          updatedAt: Date.now(),
        })),
        ...plan.entries.map((item): AutomaticRetirement => ({
          id: item.id,
          path: item.path,
          planId: plan.planId,
          status: archiveDemo === "partial" ? "failed" : "complete",
          reason:
            archiveDemo === "partial"
              ? "Checkout removed; final recovery journal write will retry. Branches are kept."
              : null,
          updatedAt: Date.now(),
        })),
      ];
      automaticJobs.set(cwd, automatic);
      void emit("worktree-retirement-changed");
      return {
        review: { planId: plan.planId, entries: [], kept: [] },
        automatic,
      };
    }
    if (command === "git_branches") {
      return {
        current: "main",
        detached: false,
        branches: [
          { name: "main", remote: null, current: true },
          { name: "develop", remote: null, current: false },
        ],
      };
    }
    if (command === "worktree_list") {
      if (scenario === "Loading")
        await new Promise((resolve) => setTimeout(resolve, 2500));
      if (scenario === "Error")
        throw new Error(
          "Could not read this repository. Check that the project folder is available.",
        );
      if (scenario === "Restoration") {
        const main = currentEntries(cwd).find((entry) => entry.main)!;
        return {
          repo: cwd,
          settings: currentSettings(cwd),
          retirementPolicy: currentPolicy(cwd),
          automaticRetirement: automaticJobs.get(cwd) ?? [],
          entries: [
            main,
            {
              ...seed(cwd)[1],
              id: `${cwd}:restore`,
              path: `${cwd}/.worktrees/restorable`,
              branch: "monocode/restorable-work",
              missing: !restorationComplete,
              blockedReason: restorationComplete
                ? "Used by an open conversation"
                : "Working folder was removed; branch is available to restore",
            },
          ],
        } satisfies WorktreeOverview;
      }
      return {
        repo: cwd,
        settings: currentSettings(cwd),
        retirementPolicy: currentPolicy(cwd),
        automaticRetirement: automaticJobs.get(cwd) ?? [],
        entries:
          scenario === "Empty"
            ? currentEntries(cwd).filter((entry) => entry.main)
            : currentEntries(cwd),
      } satisfies WorktreeOverview;
    }
    if (command === "worktree_pin") {
      for (const list of entries.values()) {
        const entry = list.find((candidate) => candidate.id === payload.id);
        if (entry) {
          entry.pinned = !!payload.pinned;
          entry.blockedReason = entry.pinned ? "Pinned" : null;
        }
      }
      return;
    }
    if (command === "worktree_settings_set") {
      const current = currentSettings(cwd);
      if (
        (payload.settings?.environmentVersion ?? 0) !==
        (current.environmentVersion ?? 0)
      ) {
        throw new Error(
          "WORKTREE_SETTINGS_CONFLICT: settings changed in another window",
        );
      }
      const saved = {
        ...current,
        ...payload.settings,
        environmentVersion: (current.environmentVersion ?? 0) + 1,
      };
      projectSettings.set(cwd, saved);
      return saved;
    }
    if (command === "worktree_storage_get") return recoveryStorage();
    if (command === "worktree_storage_limit_set") {
      if (payload.expectedVersion !== recoveryStorageVersion) {
        throw new Error(
          "WORKTREE_STORAGE_CONFLICT: storage settings changed in another window",
        );
      }
      if (
        payload.limitBytes === undefined ||
        payload.limitBytes < recoveryStorageUsed
      ) {
        throw new Error("The limit cannot be lower than current usage.");
      }
      recoveryStorageLimit = payload.limitBytes;
      recoveryStorageVersion += 1;
      void emit("worktree-storage-changed");
      return recoveryStorage();
    }
    if (command === "worktree_prepare") {
      workspaceCalls.push(command);
      return payload.request?.path ?? payload.request?.cwd;
    }
    if (command === "worktree_setup") {
      workspaceCalls.push(command);
      if (previewSetupFailure)
        throw new Error(
          "Fixture setup failed; clear the failure toggle and retry",
        );
      restorationComplete = true;
      return;
    }
    if (command === "session_set_archived") {
      workspaceCalls.push(command);
      previewArchived = !!payload.archived;
      return;
    }
    if (command === "worktree_name_status") return "waiting";
    if (command === "worktree_name") return "named";
    if (command === "worktree_heartbeat") return;
    if (command === "worktree_retirement_plan") {
      return payload.sessionIds?.length
        ? archivePlan()
        : settingsPlan(cwd, payload.ids ?? []);
    }
    if (command === "worktree_retire") {
      return retirementReport(payload.planId!, payload.selections ?? []);
    }
    // The preview has no native transport. Confirmed actions only edit fixtures.
    return null;
  },
  { shouldMockEvents: true },
);

const { createRoot } = await import("react-dom/client");
const { useEffect, useMemo, useRef, useState } = await import("react");
const { SettingsView } = await import("../surfaces/SettingsView");
const { SettingsNav } = await import("../chrome/SettingsRail");
const { WorkspacePicker } = await import("../chrome/WorkspacePicker");
const { newSession } = await import("../lib/session");
const { createSessionWorkspacePreparation } =
  await import("../lib/sessionWorkspace");
const { WorktreeRetirementDialog } =
  await import("../chrome/WorktreeRetirementDialog");
const { AppToaster } = await import("../chrome/AppToaster");
const { toast } = await import("sonner");
const { finishWorktreeNaming } = await import("../lib/worktreeNaming");
const { archiveSessionsWithRetirement } =
  await import("../lib/worktreeRetirement");
const { refreshWorktrees } = await import("../hooks/useWorktrees");
const { rememberProject, archiveProject } = await import("../lib/recents");
const { initAppearance, applyThemePreference, isLightScheme } =
  await import("../lib/appearance");
for (const path of paths) rememberProject(path);
archiveProject("/Users/demo/Archive/monocode-eric");
initAppearance();
const previewTheme = previewParams.get("theme");
if (previewTheme === "light" || previewTheme === "dark") {
  applyThemePreference(previewTheme);
}
document.documentElement.classList.remove("has-native-glass");

function Preview() {
  const [section, setSection] =
    useState<import("../lib/settings").SettingsSectionId>("worktrees");
  const [light, setLight] = useState(isLightScheme);
  const [composerSession, setComposerSession] = useState(() => ({
    ...newSession("claude", paths[0]),
    ...(previewParams.get("history") === "1"
      ? {
          transcriptOnly: true,
          worktreeCwd: `${paths[0]}/.worktrees/restorable`,
          branch: "monocode/restorable-work",
          blocks: [
            {
              id: "saved-message",
              role: "user" as const,
              text: "Saved history remains readable while its worktree is retired.",
            },
          ],
        }
      : {}),
  }));
  const composerRef = useRef(composerSession);
  composerRef.current = composerSession;
  const prepareWorkspace = useMemo(
    () =>
      createSessionWorkspacePreparation({
        current: (id) =>
          composerRef.current.id === id ? composerRef.current : undefined,
        update: (session, patch) => {
          const prepared = { ...composerRef.current, ...session, ...patch };
          composerRef.current = prepared;
          setComposerSession(prepared);
          return prepared;
        },
      }),
    [],
  );
  const [notice, setNotice] = useState("");
  const [reviewPlan, setReviewPlan] = useState<WorktreeRetirementPlan | null>(
    null,
  );
  useEffect(() => {
    if (previewParams.get("toast") === "naming") {
      let current = true;
      void finishWorktreeNaming(
        "preview-naming",
        {
          token: "preview-naming",
          result: Promise.resolve(null),
          retry: async () => "preview-worktree-name",
          isCurrent: () => current,
        },
        Promise.resolve(),
      );
      return () => {
        current = false;
        toast.dismiss("worktree-naming-preview-naming");
      };
    }
    if (previewParams.get("toast") !== "kept") return;
    const id = toast("Session archived", {
      description: "Worktree kept: Commits not merged into main",
      // Keep the sample visible while comparing themes and spacing.
      duration: Infinity,
    });
    return () => {
      toast.dismiss(id);
    };
  }, []);

  const runArchiveDemo = async (demo: ArchiveDemo) => {
    archiveDemo = demo;
    partialAttempt = 0;
    setReviewPlan(null);
    setNotice("Archiving…");
    const sessionIds =
      demo === "bulk"
        ? ["conversation-one", "conversation-two"]
        : [`conversation-${demo}`];
    await archiveSessionsWithRetirement({
      sessionIds,
      archive: async () => {
        setNotice(
          sessionIds.length === 1
            ? "Conversation archived."
            : `${sessionIds.length} conversations archived.`,
        );
        return true;
      },
      protectedPaths: () => [],
      onReview: (plan) => {
        if (plan.entries.length || plan.kept.length) {
          setReviewPlan(plan);
        } else {
          setNotice(
            "Archived. The shared worktree is still in use, so there is nothing to review.",
          );
        }
      },
      onReviewError: (cause) =>
        setNotice(`Archived. Review failed: ${String(cause)}`),
      onAutomatic: (items) => {
        if (!items.length) return;
        const problems = items.filter((item) => item.reason);
        setNotice(
          problems.length
            ? `Archived. ${problems.map((item) => item.reason).join(" ")}`
            : `Archived. ${items.length} checkout(s) retired; recovery and branches kept.`,
        );
        if (problems.length)
          toast("Archived · automatic cleanup needs attention", {
            description: problems.map((item) => item.reason).join("\n"),
            duration: Infinity,
            closeButton: true,
          });
      },
    });
  };

  return (
    <div className="flex h-full flex-col font-sans">
      <div className="flex min-h-0 flex-1">
        <aside className="sidebar-glass flex w-52 shrink-0 flex-col border-r border-content/10 pt-12">
          <SettingsNav
            section={section}
            onSelect={setSection}
            onClose={() => setSection("worktrees")}
          />
        </aside>
        <SettingsView
          section={section}
          cwd={paths[0]}
          recents={recents}
          sessions={[]}
          besideRail
          onClose={() => setSection("worktrees")}
          onOpenSession={() => {}}
          onArchiveSession={() => {}}
          onDeleteSession={() => {}}
          onOpenWhatsNew={() => {}}
          onOpenWorktree={async (_cwd, worktreeCwd) => {
            const session = { ...composerRef.current, worktreeCwd };
            composerRef.current = session;
            const prepared = await prepareWorkspace(session);
            setNotice(
              `Prepared ${prepared.cwd}; ${workspaceCalls.join(" → ")}`,
            );
            await refreshWorktrees(session.cwd);
          }}
        />
      </div>

      <footer className="shrink-0 border-t border-content/10 bg-content/3 px-4 py-2 text-[11px] text-content/50">
        <div className="mb-2 flex items-center gap-3 border-b border-content/8 pb-2">
          <span className="font-medium text-content/65">Composer preview</span>
          <WorkspacePicker
            session={composerSession}
            enabled
            onChange={(workspaceChoice) =>
              setComposerSession((current) => ({ ...current, workspaceChoice }))
            }
          />
        </div>
        {previewParams.get("history") === "1" ? (
          <div className="space-y-2 border-b border-content/8 py-2">
            <p>{composerSession.blocks[0]?.text}</p>
            <p>
              {previewArchived ? "Archived" : "Active"} ·{" "}
              {composerSession.transcriptOnly
                ? "Transcript only"
                : "Workspace in use"}
            </p>
            <label>
              <input
                type="checkbox"
                onChange={(event) => {
                  previewSetupFailure = event.target.checked;
                }}
              />{" "}
              Simulate setup failure
            </label>
            <button
              className="mx-3 text-accent"
              onClick={() => {
                const before = workspaceCalls.length;
                setComposerSession((session) => ({
                  ...session,
                  transcriptOnly: true,
                }));
                setNotice(
                  `Read history: ${workspaceCalls.length - before} workspace calls; archive state unchanged`,
                );
              }}
            >
              Read history
            </button>
            <button
              className="text-accent"
              onClick={() => {
                void prepareWorkspace(composerRef.current)
                  .then((prepared) =>
                    setNotice(
                      `Prepared ${prepared.cwd}; ${workspaceCalls.join(" → ")}`,
                    ),
                  )
                  .catch((error) =>
                    setNotice(
                      `${String(error)}; history remains readable and archived`,
                    ),
                  );
              }}
            >
              Restore workspace
            </button>
          </div>
        ) : null}
        <div className="flex flex-wrap items-center gap-2 border-b border-content/8 pb-2">
          <span className="mr-1 font-medium text-content/65">Archive demo</span>
          {(
            [
              ["shared", "Shared · no review"],
              ["final", "Final conversation"],
              ["bulk", "Bulk archive"],
              ["blocker", "Safety blocker"],
              ["partial", "Partial failure"],
            ] as const
          ).map(([demo, label]) => (
            <button
              key={demo}
              className="rounded border border-content/12 px-2 py-1 hover:bg-content/10 hover:text-content"
              onClick={() => void runArchiveDemo(demo)}
            >
              {label}
            </button>
          ))}
          {archiveDemo === "partial" && reviewPlan ? (
            <span className="text-content/40">
              Select remote deletion to preview the partial result.
            </span>
          ) : null}
        </div>
        <div className="flex flex-wrap items-center gap-2 pt-2">
          <span className="mr-auto">
            UI preview · sample data{notice ? ` · ${notice}` : ""}
          </span>
          {(
            ["Populated", "Empty", "Loading", "Error", "Restoration"] as const
          ).map((name) => (
            <button
              key={name}
              aria-pressed={scenario === name}
              className={`rounded px-2 py-1 hover:bg-content/10 ${scenario === name ? "bg-content/10 text-content" : ""}`}
              onClick={() => {
                const url = new URL(location.href);
                url.searchParams.set("state", name);
                location.assign(url);
              }}
            >
              {name}
            </button>
          ))}
          <button
            className="rounded border border-content/15 px-2 py-1"
            onClick={() => {
              applyThemePreference(light ? "dark" : "light");
              setLight(!light);
            }}
          >
            {light ? "Dark theme" : "Light theme"}
          </button>
        </div>
      </footer>

      <AppToaster />
      {reviewPlan ? (
        <WorktreeRetirementDialog
          plan={reviewPlan}
          source="archive"
          onClose={() => setReviewPlan(null)}
          onRetired={(report) => {
            const failures = report.results.filter(
              (result) => result.error,
            ).length;
            setNotice(
              failures
                ? `Archived. ${failures} worktree has unfinished cleanup.`
                : "Archived and retired. Recovery details are shown in the review.",
            );
            void refreshWorktrees(paths[0]);
          }}
        />
      ) : null}
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<Preview />);
