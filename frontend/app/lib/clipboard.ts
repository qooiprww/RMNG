/** Copy `text` to the clipboard, returning whether it actually landed there.
 *
 *  `navigator.clipboard` only exists in a **secure context** (HTTPS or
 *  `http://localhost`). The control-server UI is commonly opened over plain HTTP at
 *  `http://<lan-ip>:9000`, where `navigator.clipboard` is `undefined` — so we fall
 *  back to the legacy hidden-`<textarea>` + `document.execCommand("copy")` path,
 *  which works in insecure contexts. Callers must only report success when this
 *  resolves `true` (otherwise the write silently failed). */
export async function copyText(text: string): Promise<boolean> {
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // Fall through to the execCommand path (permissions/insecure-context refusal).
    }
  }
  return legacyCopy(text);
}

/** Legacy synchronous copy: stage the text in an off-screen textarea, select it, and
 *  ask the document to copy the selection. Returns `false` if unsupported or blocked. */
function legacyCopy(text: string): boolean {
  if (typeof document === "undefined") return false;
  const ta = document.createElement("textarea");
  ta.value = text;
  // Keep it out of view + non-interactive so selecting it doesn't scroll or flicker.
  ta.setAttribute("readonly", "");
  ta.style.position = "fixed";
  ta.style.top = "-9999px";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  try {
    ta.select();
    ta.setSelectionRange(0, text.length);
    return document.execCommand("copy");
  } catch {
    return false;
  } finally {
    document.body.removeChild(ta);
  }
}
