import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const source = readFileSync(new URL("./connections-view.tsx", import.meta.url), "utf8");

describe("governed provider connection controls", () => {
  test("warns that disconnect affects every present and future runtime", () => {
    expect(source).toContain("all present and future agent runtimes");
    expect(source).toContain("same Steward identity");
  });

  test("distinguishes an outstanding OAuth flow from an ordinary conflict", () => {
    expect(source).toContain('oauth_flow_pending');
    expect(source).toContain("Finish or wait for the pending GitHub authorization");
  });
});
