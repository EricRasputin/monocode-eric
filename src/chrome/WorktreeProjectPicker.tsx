import { useEffect, useId, useRef, useState } from "react";
import { prettyCwd, projectKey, projectName } from "../lib/paths";
import { Check, ChevronDown, Folder, Search } from "./icons";
import { Popover } from "./Popover";

export type CleanupProject = { path: string; archived: boolean };

/** Uses the same searchable, themed popover as the workspace and model pickers. */
export function WorktreeProjectPicker({
  cwd,
  projects,
  disabled,
  autoFocus,
  onSelect,
}: {
  cwd: string;
  projects: CleanupProject[];
  disabled: boolean;
  autoFocus: boolean;
  onSelect: (key: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const trigger = useRef<HTMLButtonElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const activeOption = useRef<HTMLButtonElement>(null);
  const listId = useId();
  const filtered = projects.filter((project) =>
    project.path.toLowerCase().includes(query.toLowerCase()),
  );
  const selected = projects.find(
    (project) => projectKey(project.path) === projectKey(cwd),
  );
  useEffect(() => {
    if (open) search.current?.focus();
  }, [open]);
  useEffect(() => {
    if (open) activeOption.current?.scrollIntoView({ block: "nearest" });
  }, [active, open]);
  const show = () => {
    setQuery("");
    setActive(
      Math.max(
        0,
        projects.findIndex(
          (project) => projectKey(project.path) === projectKey(cwd),
        ),
      ),
    );
    setOpen(true);
  };
  const pick = (path: string) => {
    setOpen(false);
    trigger.current?.focus();
    onSelect(projectKey(path));
  };
  return (
    <div className="min-w-0 flex-1">
      <button
        ref={trigger}
        type="button"
        autoFocus={autoFocus}
        disabled={disabled}
        aria-label={`Project: ${projectName(cwd)}`}
        aria-haspopup="dialog"
        aria-expanded={open}
        title={cwd}
        onClick={() => (open ? setOpen(false) : show())}
        onKeyDown={(event) => {
          if (event.key === "ArrowDown") {
            event.preventDefault();
            show();
          }
        }}
        className="flex max-w-full items-center gap-2 rounded-md border border-content/10 bg-content/5 px-2.5 py-1.5 text-[13px] font-medium text-content hover:border-content/20 hover:bg-content/8 focus-visible:outline-1 focus-visible:outline-accent disabled:opacity-40"
      >
        <Folder
          className="size-4 shrink-0 text-content/50"
          strokeWidth={1.75}
        />
        <span className="truncate">{projectName(cwd)}</span>
        {selected?.archived && (
          <span className="text-[10px] font-normal text-content/40">
            Archived
          </span>
        )}
        <ChevronDown
          className="size-3 shrink-0 text-content/40"
          strokeWidth={1.75}
        />
      </button>
      {open && (
        <Popover
          anchor={trigger}
          width={340}
          maxHeight={350}
          role="dialog"
          aria-label="Choose project"
          onDismiss={(reason) => {
            setOpen(false);
            if (reason === "escape") trigger.current?.focus();
          }}
        >
          <div className="flex items-center gap-2 border-b border-content/10 px-3 py-2.5">
            <Search className="size-3.5 shrink-0 text-content/40" />
            <input
              ref={search}
              role="combobox"
              aria-label="Find project"
              aria-expanded="true"
              aria-controls={listId}
              aria-autocomplete="list"
              aria-activedescendant={
                filtered[active] ? `${listId}-${active}` : undefined
              }
              placeholder="Find a project…"
              value={query}
              onChange={(event) => {
                setQuery(event.target.value);
                setActive(0);
              }}
              onKeyDown={(event) => {
                if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                  event.preventDefault();
                  setActive((index) =>
                    Math.max(
                      0,
                      Math.min(
                        filtered.length - 1,
                        index + (event.key === "ArrowDown" ? 1 : -1),
                      ),
                    ),
                  );
                } else if (event.key === "Enter") {
                  event.preventDefault();
                  if (filtered[active]) pick(filtered[active].path);
                } else if (event.key === "Tab") {
                  setOpen(false);
                  trigger.current?.focus();
                }
              }}
              className="min-w-0 flex-1 bg-transparent text-[12px] text-content outline-none placeholder:text-content/35"
            />
          </div>
          <div
            id={listId}
            role="listbox"
            aria-label="Projects"
            className="max-h-64 overflow-y-auto overscroll-contain p-1"
          >
            {filtered.map((project, index) => (
              <button
                type="button"
                key={projectKey(project.path)}
                id={`${listId}-${index}`}
                ref={active === index ? activeOption : undefined}
                role="option"
                tabIndex={-1}
                aria-selected={projectKey(project.path) === projectKey(cwd)}
                onMouseDown={(event) => event.preventDefault()}
                onMouseEnter={() => setActive(index)}
                onClick={() => pick(project.path)}
                className={`flex w-full items-center gap-2.5 rounded-lg px-2.5 py-2 text-left ${index === active ? "bg-content/10" : "hover:bg-content/5"}`}
              >
                <Folder
                  className="size-4 shrink-0 text-content/45"
                  strokeWidth={1.75}
                />
                <span className="min-w-0 flex-1">
                  <span className="flex items-center gap-2 text-[12px] text-content/90">
                    <span className="truncate">
                      {projectName(project.path)}
                    </span>
                    {project.archived && (
                      <span className="text-[10px] text-content/40">
                        Archived
                      </span>
                    )}
                  </span>
                  <span className="mt-0.5 block truncate text-[11px] text-content/40">
                    {prettyCwd(project.path)}
                  </span>
                </span>
                {projectKey(project.path) === projectKey(cwd) && (
                  <Check className="size-3.5 shrink-0 text-content/65" />
                )}
              </button>
            ))}
            {!filtered.length && (
              <p className="px-3 py-6 text-center text-[12px] text-content/45">
                No matching projects
              </p>
            )}
          </div>
        </Popover>
      )}
    </div>
  );
}
