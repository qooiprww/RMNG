// Configure a host's local port forwards (remote clone port → 127.0.0.1:<local> on the
// machine running the native viewer). Mirrors the change-account modal shell. Live
// status (listening / error / offline) is merged from the `forwards` SSE event by rule id.
import { useState } from "react";

import type { Host } from "~/lib/types";
import type { ForwardRuntime } from "~/lib/wire/ForwardRuntime";

type Row = { id?: string; remotePort: string; localPort: string; label: string; enabled: boolean };

function toRows(host: Host): Row[] {
  return (host.forwards ?? []).map((f) => ({
    id: f.id,
    remotePort: String(f.remotePort),
    localPort: String(f.localPort),
    label: f.label ?? "",
    enabled: f.enabled,
  }));
}

function statusFor(runtime: ForwardRuntime[], id?: string): ForwardRuntime | undefined {
  return id ? runtime.find((r) => r.id === id) : undefined;
}

function Badge({ rt }: { rt?: ForwardRuntime }) {
  const state = rt?.state ?? "offline";
  const color =
    state === "listening"
      ? "bg-emerald-100 text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300"
      : state === "error"
        ? "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300"
        : "bg-slate-100 text-slate-500 dark:bg-slate-700 dark:text-slate-400";
  return (
    <span className={`rounded px-1.5 py-0.5 text-[10px] font-medium ${color}`} title={rt?.error ?? ""}>
      {state}
      {rt && rt.activeConns > 0 ? ` · ${rt.activeConns}` : ""}
    </span>
  );
}

export function PortForwardModal({
  host,
  runtime,
  busy,
  error,
  onClose,
  onSubmit,
}: {
  host: Host;
  runtime: ForwardRuntime[];
  busy: boolean;
  error: string | null;
  onClose: () => void;
  onSubmit: (
    forwards: Array<{ id?: string; remotePort: number; localPort: number; enabled: boolean; label?: string }>,
  ) => void;
}) {
  const [rows, setRows] = useState<Row[]>(() => toRows(host));

  const update = (i: number, patch: Partial<Row>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const remove = (i: number) => setRows((rs) => rs.filter((_, j) => j !== i));
  const add = () =>
    setRows((rs) => [...rs, { remotePort: "", localPort: "", label: "", enabled: true }]);

  const submit = () => {
    const forwards = rows.map((r) => ({
      id: r.id,
      remotePort: Number(r.remotePort),
      localPort: Number(r.localPort),
      enabled: r.enabled,
      label: r.label.trim() || undefined,
    }));
    onSubmit(forwards);
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-slate-900/30 p-4" onClick={onClose}>
      <div
        className="w-full max-w-lg rounded-xl border border-slate-200 bg-white p-5 shadow-xl dark:border-slate-700 dark:bg-slate-800"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
        }}
      >
        <h3 className="text-sm font-semibold text-slate-900 dark:text-slate-100">
          Port forwards · <span className="text-emerald-700 dark:text-emerald-400">{host.displayName ?? host.id}</span>
        </h3>
        <p className="mt-1 text-xs text-slate-500 dark:text-slate-400">
          Expose a port inside this host at <code>127.0.0.1:&lt;local&gt;</code> on the machine running the viewer.
        </p>

        {error ? <p className="mt-3 text-xs text-red-600 dark:text-red-400">{error}</p> : null}

        <div className="mt-4 space-y-2">
          <div className="grid grid-cols-[1fr_1fr_1fr_auto_auto] gap-2 text-[11px] font-medium uppercase tracking-wide text-slate-400">
            <span>Remote</span>
            <span>Local</span>
            <span>Label</span>
            <span>On</span>
            <span></span>
          </div>
          {rows.map((r, i) => (
            <div key={i} className="grid grid-cols-[1fr_1fr_1fr_auto_auto] items-center gap-2">
              <input
                inputMode="numeric"
                value={r.remotePort}
                onChange={(e) => update(i, { remotePort: e.target.value, id: undefined })}
                placeholder="3000"
                className="rounded-md border border-slate-300 px-2 py-1 text-sm dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
              />
              <div className="flex items-center gap-1">
                <input
                  inputMode="numeric"
                  value={r.localPort}
                  onChange={(e) => update(i, { localPort: e.target.value, id: undefined })}
                  placeholder="8080"
                  className="w-full rounded-md border border-slate-300 px-2 py-1 text-sm dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
                />
                <Badge rt={statusFor(runtime, r.id)} />
              </div>
              <input
                value={r.label}
                onChange={(e) => update(i, { label: e.target.value })}
                placeholder="dev server"
                className="rounded-md border border-slate-300 px-2 py-1 text-sm dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
              />
              <input
                type="checkbox"
                checked={r.enabled}
                onChange={(e) => update(i, { enabled: e.target.checked })}
                className="size-4"
              />
              <button
                type="button"
                onClick={() => remove(i)}
                className="cursor-pointer rounded px-1.5 py-1 text-xs text-red-600 hover:bg-red-50 dark:text-red-400 dark:hover:bg-red-950/40"
              >
                ✕
              </button>
            </div>
          ))}
          <button
            type="button"
            onClick={add}
            className="cursor-pointer text-xs text-emerald-700 hover:underline dark:text-emerald-400"
          >
            + Add forward
          </button>
        </div>

        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-3 py-1.5 text-sm text-slate-600 hover:bg-slate-100 dark:text-slate-300 dark:hover:bg-slate-800"
          >
            Cancel
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={submit}
            className="rounded-md bg-emerald-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-emerald-500 disabled:opacity-50"
          >
            {busy ? "Saving…" : "Save"}
          </button>
        </div>
      </div>
    </div>
  );
}
