import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";

import claudeLogo from "../assets/claude.png";
import type { Host, Operation } from "~/lib/types";
import type { ContainerStats } from "~/lib/wire/ContainerStats";
import { workspaceBadge } from "~/lib/workspace";

// Visual style per host state. A running host is `working` (sky, pulsing) or
// `idle` (amber — done / awaiting the next task / needs you); `offline` is rose.
const AGENT_STATUS: Record<
  NonNullable<Host["monitorState"]>,
  { dot: string; text: string; label: string; pulse?: boolean }
> = {
  working: { dot: "bg-sky-500", text: "text-sky-600", label: "working", pulse: true },
  idle: { dot: "bg-amber-500", text: "text-amber-700", label: "idle" },
  offline: { dot: "bg-rose-400", text: "text-rose-500", label: "offline" },
};

function effectiveStatus(host: Host): { dot: string; text: string; label: string; pulse?: boolean } {
  return AGENT_STATUS[host.monitorState ?? "idle"];
}

/** How this clone's Claude account was chosen + which account is actually in use, for
 *  the sidebar line. `mode` is the selection kind; `email` is the live account (absent
 *  for "none", or when auto/specific hasn't resolved one). Returns null when there's
 *  nothing Claude-related to show (e.g. auto with no accounts configured). */
type ClaudeSel = { mode: "auto" | "group" | "specific" | "none"; group?: string; email?: string };
function claudeSelection(host: Host): ClaudeSel | null {
  const email = host.claudeAccountEmail || undefined;
  const sel = host.claudeSelection;
  if (host.claudeGroup) return { mode: "group", group: host.claudeGroup, email };
  if (sel === "none") return { mode: "none" };
  if (sel === "auto") return email ? { mode: "auto", email } : null;
  if (sel && !sel.startsWith("group:")) return { mode: "specific", email: email ?? sel };
  // Legacy host (no selection recorded): show the account, without a mode badge.
  return email ? { mode: "specific", email } : null;
}

/** Short badge for the non-default selection modes (specific renders as the plain
 *  email, so it — and legacy hosts — get no badge). Group shows the group name. */
function selBadge(sel: ClaudeSel): string | null {
  if (sel.mode === "auto") return "auto";
  if (sel.mode === "none") return "none";
  if (sel.mode === "group") return sel.group ?? "group";
  return null; // specific
}

/** Compact live-usage string for the row, e.g. `2.4 cpu · 5.1 / 16.0 GiB`. CPU is shown
 *  in "cores" (the wire value is percent-of-one-core, so ÷100); RAM as used / limit GiB,
 *  one decimal each. Returns null when there's nothing usable to show — no stats sampled
 *  yet, a stopped/unmanaged host (no entry), or no memory limit — so the row shows nothing
 *  rather than a `0.0 / 0.0` placeholder. `mem*` are typed bigint by ts-rs but arrive as
 *  JSON numbers, hence the `Number()` coercion. */
function usageLine(stats?: ContainerStats): string | null {
  if (!stats) return null;
  const memLimit = Number(stats.memLimit);
  if (memLimit <= 0) return null;
  const GiB = 1024 ** 3;
  const cores = (stats.cpuPct / 100).toFixed(1);
  const used = (Number(stats.memUsed) / GiB).toFixed(1);
  const limit = (memLimit / GiB).toFixed(1);
  return `${cores} cpu · ${used} / ${limit} GiB`;
}

function selTitle(sel: ClaudeSel): string {
  switch (sel.mode) {
    case "group":
      return `Claude group: ${sel.group} (on ${sel.email ?? "?"})`;
    case "auto":
      return `Claude: auto — server picks the best account${sel.email ? ` (on ${sel.email})` : ""}`;
    case "none":
      return "Claude: none — no token installed";
    default:
      return `Claude account: ${sel.email} (fixed)`;
  }
}

export interface SidebarHostProps {
  host: Host;
  /** Live CPU/RAM usage for this host's container, pushed over the `stats` SSE event.
   *  Absent for a stopped/unmanaged host or before the first sample — renders nothing. */
  stats?: ContainerStats;
  selected: boolean;
  /** A running operation targeting this host (delete, or a clone finishing its
   *  post-add `wait-swap` step), if any. */
  op?: Operation;
  onSelect: () => void;
  onDelete: () => void;
  /** Commit this managed clone to a new clone-source image. */
  onCommit: () => void;
  /** Change this clone's Claude account/group. */
  onChangeAccount: () => void;
}

function GripIcon() {
  return (
    <svg width="12" height="12" viewBox="0 0 16 16" fill="currentColor" aria-hidden>
      <circle cx="5" cy="3" r="1.4" />
      <circle cx="11" cy="3" r="1.4" />
      <circle cx="5" cy="8" r="1.4" />
      <circle cx="11" cy="8" r="1.4" />
      <circle cx="5" cy="13" r="1.4" />
      <circle cx="11" cy="13" r="1.4" />
    </svg>
  );
}

function CameraIcon() {
  return (
    <svg
      width="13"
      height="13"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinejoin="round"
      aria-hidden
    >
      <path d="M2 5.5A1.5 1.5 0 0 1 3.5 4h1l.9-1.2A1 1 0 0 1 6.2 2.4h3.6a1 1 0 0 1 .8.4L11.5 4h1A1.5 1.5 0 0 1 14 5.5v6A1.5 1.5 0 0 1 12.5 13h-9A1.5 1.5 0 0 1 2 11.5z" />
      <circle cx="8" cy="8.2" r="2.3" />
    </svg>
  );
}

function AccountIcon() {
  return (
    <svg
      width="13"
      height="13"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <path d="M2.5 6h9M9.5 4l2 2-2 2" />
      <path d="M13.5 10h-9M6.5 8l-2 2 2 2" />
    </svg>
  );
}

export function SidebarHost({
  host,
  stats,
  selected,
  op,
  onSelect,
  onDelete,
  onCommit,
  onChangeAccount,
}: SidebarHostProps) {
  const busy = op?.status === "running";
  // Managed clones (backed by a container named after the host id) get the commit /
  // account actions; plain unmanaged rows only get delete.
  const managed = host.managed === true;
  const status = effectiveStatus(host);
  const claudeSel = claudeSelection(host);
  const usage = usageLine(stats);
  const { attributes, listeners, setNodeRef, transform, transition, isDragging } =
    useSortable({ id: host.id, disabled: busy });

  const style: React.CSSProperties = {
    transform: CSS.Transform.toString(transform),
    transition,
    zIndex: isDragging ? 10 : undefined,
  };

  return (
    // The whole row is the select target, so the click area matches the hover
    // highlight. The grip + clone/delete buttons stop propagation.
    <div
      ref={setNodeRef}
      style={style}
      role="button"
      tabIndex={0}
      aria-pressed={selected}
      onClick={onSelect}
      onKeyDown={(e) => {
        if (e.target === e.currentTarget && (e.key === "Enter" || e.key === " ")) {
          e.preventDefault();
          onSelect();
        }
      }}
      title={`${host.id} · ${host.host}:${host.port}`}
      className={`group flex cursor-pointer items-center gap-1 rounded-lg border px-1.5 py-1.5 ${
        selected
          ? "border-emerald-300 bg-emerald-50"
          : "border-transparent hover:bg-slate-100"
      } ${isDragging ? "border-slate-200 bg-white shadow-md" : ""}`}
    >
      <button
        type="button"
        aria-label="drag to reorder"
        {...attributes}
        {...listeners}
        onClick={(e) => e.stopPropagation()}
        disabled={busy}
        className="cursor-grab touch-none rounded p-1 text-slate-300 hover:text-slate-500 active:cursor-grabbing disabled:opacity-0"
      >
        <GripIcon />
      </button>

      <div className="min-w-0 flex-1">
        {!busy && claudeSel ? (
          <p
            className="mb-0.5 flex items-center gap-1 text-[10px] text-slate-400"
            title={selTitle(claudeSel)}
          >
            <img src={claudeLogo} alt="" className="h-3 w-3 shrink-0 object-contain" />
            {selBadge(claudeSel) ? (
              <span className="shrink-0 rounded bg-slate-100 px-1 text-[9px] font-semibold text-slate-500">
                {selBadge(claudeSel)}
              </span>
            ) : null}
            {claudeSel.email ? (
              <span className="truncate">{claudeSel.email}</span>
            ) : claudeSel.mode === "none" ? (
              <span className="italic text-slate-300">no token</span>
            ) : null}
          </p>
        ) : null}
        <span className="flex items-center gap-1.5">
          {!busy ? (
            <span
              className={`h-2 w-2 shrink-0 rounded-full ${status.dot} ${
                status.pulse ? "animate-pulse" : ""
              }`}
              aria-hidden
            />
          ) : null}
          {host.linearWorkspace && host.linearTicket ? (
            <span
              className={`shrink-0 rounded px-1 py-0.5 text-[10px] font-semibold leading-none ${workspaceBadge(
                host.linearWorkspace,
              )}`}
            >
              {host.linearTicket}
            </span>
          ) : null}
          <span className="min-w-0 flex-1 truncate text-sm font-medium text-slate-800">
            {host.displayName ?? host.id}
          </span>
          {!busy && host.unread && !selected ? (
            <span
              className="ml-auto h-2 w-2 shrink-0 rounded-full bg-indigo-500"
              title="stopped working since you last viewed it"
              aria-label="unread: stopped working since last viewed"
            />
          ) : null}
          {busy ? (
            <span className="ml-auto shrink-0 text-[10px] font-medium text-sky-600">
              {op?.kind === "delete" ? "deleting…" : op?.step}
            </span>
          ) : null}
        </span>
        {!busy ? (
          <p
            className={`mt-1 line-clamp-2 text-xs leading-snug ${status.text}`}
            title={host.stateNote || status.label}
          >
            {[host.linearLabel, host.stateNote || status.label].filter(Boolean).join(" · ")}
          </p>
        ) : null}
        {!busy && usage ? (
          <p
            className="mt-0.5 text-[10px] tabular-nums text-slate-400"
            title="live container CPU (cores) · memory used / limit"
          >
            {usage}
          </p>
        ) : null}
      </div>

      {managed ? (
        <>
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onCommit();
            }}
            disabled={busy}
            aria-label={`commit ${host.id} to an image`}
            title="commit to a clone-source image"
            className="rounded p-1 text-slate-400 opacity-0 hover:bg-emerald-50 hover:text-emerald-600 group-hover:opacity-100 disabled:opacity-0"
          >
            <CameraIcon />
          </button>
          <button
            type="button"
            onClick={(e) => {
              e.stopPropagation();
              onChangeAccount();
            }}
            disabled={busy}
            aria-label={`change Claude account for ${host.id}`}
            title="change Claude account / group"
            className="rounded p-1 text-slate-400 opacity-0 hover:bg-emerald-50 hover:text-emerald-600 group-hover:opacity-100 disabled:opacity-0"
          >
            <AccountIcon />
          </button>
        </>
      ) : null}

      {/* Every host is deletable (managed clones destroy the container; plain rows
          just unregister). */}
      <button
        type="button"
        onClick={(e) => {
          e.stopPropagation();
          onDelete();
        }}
        disabled={busy}
        aria-label={`delete ${host.id}`}
        className="rounded px-1 text-base leading-none text-slate-400 opacity-0 hover:text-red-600 group-hover:opacity-100 disabled:opacity-0"
      >
        ×
      </button>
    </div>
  );
}
