import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const source = readFileSync(new URL("./admin-envelope-provision-view.tsx", import.meta.url), "utf8");

describe("administrator envelope provisioning", () => {
  test("summarizes the template's public egress hosts before provisioning", () => {
    expect(source).toContain('["Public egress (read-only)", template.envelope.spec.egress?.hosts');
  });
});
