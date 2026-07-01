// Provision a brand-new template CT from the configured base image. Unlike a
// clone (which copies an existing template), this bootstraps from zero using the
// resources/base image set in Settings → "Clone container". The server takes only
// a hostname; everything else comes from config.template.
import { useState } from "react";

/** Mirrors the server's `is_dns_label`: non-empty, ≤63 chars, lowercase letters /
 *  digits / hyphens, no leading or trailing hyphen. */
function isDnsLabel(s: string): boolean {
  return s.length <= 63 && /^[a-z0-9]([a-z0-9-]*[a-z0-9])?$/.test(s);
}

export function NewTemplateModal({
  busy,
  existing,
  onClose,
  onCreate,
}: {
  busy: boolean;
  /** Existing host ids, to flag a duplicate name before the server does. */
  existing: Set<string>;
  onClose: () => void;
  onCreate: (hostname: string) => void;
}) {
  const [hostname, setHostname] = useState("");
  const trimmed = hostname.trim();
  const labelOk = isDnsLabel(trimmed);
  const duplicate = existing.has(trimmed);
  const valid = labelOk && !duplicate;

  function submit() {
    if (!valid || busy) return;
    onCreate(trimmed);
  }

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-slate-900/30 p-4"
      onClick={onClose}
    >
      <div
        className="w-full max-w-md rounded-xl border border-slate-200 bg-white p-5 shadow-xl"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
        }}
      >
        <h3 className="text-sm font-semibold text-slate-900">New template</h3>
        <p className="mt-1 text-xs text-slate-500">
          Provisions a fresh container from the base image and resources in{" "}
          <span className="font-medium text-slate-600">Settings → Clone container</span>. The new
          container is registered as a clonable template.
        </p>

        <label className="mt-4 block text-xs font-medium text-slate-600">
          Hostname
          <input
            autoFocus
            value={hostname}
            onChange={(e) => setHostname(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") submit();
            }}
            placeholder="e.g. rmng-template"
            spellCheck={false}
            className="mt-1 w-full rounded-md border border-slate-300 px-3 py-2 text-sm font-normal text-slate-900 placeholder:text-slate-400 focus:border-emerald-500 focus:outline-none"
          />
          {trimmed && !labelOk ? (
            <p className="mt-1 text-[11px] font-normal text-red-600">
              lowercase letters, digits and hyphens only (no leading/trailing hyphen, ≤63 chars)
            </p>
          ) : duplicate ? (
            <p className="mt-1 text-[11px] font-normal text-red-600">
              a host named “{trimmed}” already exists
            </p>
          ) : null}
        </label>

        <div className="mt-4 flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-3 py-1.5 text-sm text-slate-600 hover:bg-slate-100"
          >
            Cancel
          </button>
          <button
            type="button"
            onClick={submit}
            disabled={!valid || busy}
            className="rounded-md bg-emerald-600 px-4 py-1.5 text-sm font-medium text-white hover:bg-emerald-700 disabled:opacity-40"
          >
            {busy ? "Provisioning…" : "Create template"}
          </button>
        </div>
      </div>
    </div>
  );
}
