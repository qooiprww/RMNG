import { expect, test } from "bun:test";

import { currentCodexValue, currentValue } from "./ChangeAccountModal";
import type { Host } from "~/lib/types";

const host = (overrides: Partial<Host> = {}): Host => ({
  id: "h1",
  host: "h1",
  port: 3389,
  username: "rmng",
  password: "rmng",
  managed: true,
  ...overrides,
});

test("tokenless legacy Claude host is not treated as already auto", () => {
  const h = host();

  expect(currentValue(h)).toBe("none");
  expect("auto" !== currentValue(h)).toBe(true);
});

test("tokenless legacy Codex host is not treated as already auto", () => {
  const h = host();

  expect(currentCodexValue(h)).toBe("none");
  expect("auto" !== currentCodexValue(h)).toBe(true);
});
