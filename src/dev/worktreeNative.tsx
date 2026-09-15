import { isTauri } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { WorktreeRetirementDialog } from "../chrome/WorktreeRetirementDialog";
import { WorktreeRecoveryStorage } from "../chrome/WorktreeRecoveryStorage";
import { newSession, type Session } from "../lib/session";
import { rememberProject } from "../lib/recents";
import {
  getSession,
  listSessionsByProject,
  setSessionArchived,
  upsertSession,
} from "../lib/sessionStore";
import {
  archiveSessionsWithRetirement,
  resumeArchivedWorktreeSession,
} from "../lib/worktreeRetirement";
import {
  createWorktree,
  heartbeatWorktrees,
  listWorktrees,
  planWorktreeRetirement,
  saveWorktreeSettings,
  type WorktreeRetirementPlan,
} from "../lib/worktrees";
import "../index.css";

if (!import.meta.env.DEV || !isTauri()) {
  throw new Error(
    "Native worktree verification requires Tauri development mode.",
  );
}

// This development entry is deliberately restricted to disposable fixtures.
const parameters = new URLSearchParams(location.search);
const fixtureRepo = parameters.get("repo") ?? "";
const verifyEnvironment =
  new URLSearchParams(location.search).get("environment") === "1";
if (
  !/^\/(?:private\/)?tmp\/monocode-retirement-native-[^/]+\/repo$/.test(
    fixtureRepo,
  )
) {
  throw new Error(
    "Provide a disposable monocode-retirement-native fixture repository.",
  );
}
const project = parameters.get("project");
const verifyStorage = parameters.get("storage") === "1";
if (project && project !== "web" && project !== "api") {
  throw new Error("Choose a known disposable monorepo project: web or api.");
}
const repo = project ? `${fixtureRepo}/apps/${project}` : fixtureRepo;

function NativeWorktreeVerification() {
  const [sessions, setSessions] = useState<Session[]>([]);
  const [archived, setArchived] = useState<string[]>([]);
  const [plan, setPlan] = useState<WorktreeRetirementPlan | null>(null);
  const [events, setEvents] = useState<string[]>([]);
  const [busy, setBusy] = useState(true);
  const log = (text: string) => setEvents((current) => [...current, text]);
  useEffect(() => {
    let active = true;
    void listSessionsByProject(repo)
      .then(async (rows) => {
        const fixtures = rows.filter((row) =>
          row.title.startsWith("Verification conversation"),
        );
        const restored = (
          await Promise.all(fixtures.map((row) => getSession(row.id)))
        )
          .filter((session): session is Session => session != null)
          .sort((a, b) => a.title.localeCompare(b.title));
        if (!active) return;
        setSessions(restored);
        setArchived(
          fixtures.filter((row) => row.archived).map((row) => row.id),
        );
      })
      .catch((error) => {
        if (active) log(`ERROR: ${String(error)}`);
      })
      .finally(() => {
        if (active) setBusy(false);
      });
    return () => {
      active = false;
    };
  }, []);
  const run = async (operation: () => Promise<void>) => {
    setBusy(true);
    try {
      await operation();
    } catch (error) {
      log(`ERROR: ${String(error)}`);
    } finally {
      setBusy(false);
    }
  };

  const prepare = () =>
    run(async () => {
      if (verifyEnvironment) {
        const current = await listWorktrees(repo);
        await saveWorktreeSettings(repo, {
          ...current.settings,
          isolateByDefault: true,
          setupCommand: "npm ci --ignore-scripts && npm run build",
          copyPaths: [".env"],
          disposablePaths: ["node_modules", "dist"],
        });
        log(
          "Configured dependency setup, .env copying and disposable build folders",
        );
      }
      const first = newSession("claude", repo);
      const path = await createWorktree(
        repo,
        first.id,
        "Native retirement verification",
        "main",
      );
      const saved = [first, newSession("claude", repo)].map(
        (session, index) => ({
          ...session,
          worktreeCwd: path,
          title: `Verification conversation ${index + 1}`,
          blocks: [
            {
              id: crypto.randomUUID(),
              role: "user" as const,
              text: "Disposable worktree lifecycle verification",
            },
          ],
        }),
      );
      for (const session of saved) await upsertSession(session);
      setSessions(saved);
      const overview = await listWorktrees(repo);
      log(
        `Prepared ${JSON.stringify(overview.entries.find((entry) => path === entry.path || path.startsWith(`${entry.path}/`)))}`,
      );
    });

  const archive = (session: Session) =>
    run(async () => {
      await archiveSessionsWithRetirement({
        sessionIds: [session.id],
        archive: async (id) => {
          await setSessionArchived(id, true);
          setArchived((current) => [...current, id]);
          log(`Archived ${id}`);
          return true;
        },
        // This verification page has no editor, terminal or live conversation.
        protectedPaths: () => [],
        onReview: (review) => {
          log(
            `Review: ${review.entries.length} eligible, ${review.kept.length} kept`,
          );
          if (review.entries.length || review.kept.length) setPlan(review);
        },
        onReviewError: (error) => log(`Review error: ${String(error)}`),
      });
    });

  return (
    <main className="mx-auto max-w-3xl space-y-5 p-10 text-content">
      <h1 className="text-2xl font-semibold">Native worktree verification</h1>
      <p className="text-sm text-content/60">
        Disposable Git repository: {repo}
      </p>
      <div className="flex flex-wrap gap-3">
        <a href="/" onClick={() => rememberProject(repo)}>
          Open Monocode app
        </a>
        {project ? (
          <a
            href={`?${new URLSearchParams({ repo: fixtureRepo, project: project === "web" ? "api" : "web", environment: "1" })}`}
          >
            Open {project === "web" ? "api" : "web"} fixture
          </a>
        ) : null}
        <button
          disabled={busy || archived.length === 0}
          onClick={() =>
            void run(async () => {
              await heartbeatWorktrees([]);
              const review = await planWorktreeRetirement({
                sessionIds: archived,
              });
              log(
                `Review: ${review.entries.length} eligible, ${review.kept.length} kept`,
              );
              if (review.entries.length || review.kept.length) setPlan(review);
            })
          }
        >
          Review archived worktrees
        </button>
        <button
          disabled={busy || sessions.length > 0}
          onClick={() => void prepare()}
        >
          Prepare shared worktree
        </button>
        {sessions.map((session, index) => (
          <button
            key={session.id}
            disabled={busy || archived.includes(session.id)}
            onClick={() => void archive(session)}
          >
            Archive conversation {index + 1}
          </button>
        ))}
        <button
          disabled={busy || !sessions[0]}
          onClick={() =>
            void run(async () => {
              await resumeArchivedWorktreeSession(sessions[0]);
              setArchived((current) =>
                current.filter((id) => id !== sessions[0].id),
              );
              log(`Restored ${sessions[0].worktreeCwd}`);
            })
          }
        >
          Restore conversation 1
        </button>
      </div>
      {verifyStorage ? <WorktreeRecoveryStorage projectCwd={repo} /> : null}
      <div
        role="log"
        className="whitespace-pre-wrap break-all rounded-xl border border-content/10 p-4 text-xs leading-relaxed"
      >
        {events.map((event, index) => (
          <pre key={index} className="mb-3 whitespace-pre-wrap break-all">
            {event}
          </pre>
        ))}
      </div>
      {plan ? (
        <WorktreeRetirementDialog
          key={plan.planId}
          plan={plan}
          source="archive"
          onClose={() => setPlan(null)}
          onRetired={(report) =>
            log(`Retirement result: ${JSON.stringify(report)}`)
          }
        />
      ) : null}
    </main>
  );
}

createRoot(document.getElementById("root")!).render(
  <NativeWorktreeVerification />,
);
