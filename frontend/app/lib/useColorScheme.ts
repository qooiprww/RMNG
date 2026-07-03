import { useEffect, useState } from "react";

/** Live `prefers-color-scheme` — `"dark"` when the OS is in dark mode, else `"light"`.
 *  Tailwind's `dark:` variant handles our own utility classes automatically via the
 *  same media query; this hook is for third-party widgets (e.g. BlockNote) that theme
 *  through a JS prop instead of CSS, so they can follow the system setting too.
 *  Starts `"light"` on the server / first paint and corrects after mount. */
export function useColorScheme(): "light" | "dark" {
  const [scheme, setScheme] = useState<"light" | "dark">("light");
  useEffect(() => {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () => setScheme(mq.matches ? "dark" : "light");
    apply();
    mq.addEventListener("change", apply);
    return () => mq.removeEventListener("change", apply);
  }, []);
  return scheme;
}
