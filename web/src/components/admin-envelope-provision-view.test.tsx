import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const source = readFileSync(new URL("./admin-envelope-provision-view.tsx", import.meta.url), "utf8");

describe("administrator envelope provisioning", () => {
  test("provisions only the web access hosts the administrator picks, none by default", () => {
    expect(source).toContain("useState<ReadonlySet<string>>(() => new Set())");
    expect(source).toContain("requestedEnvelope: provisionedEnvelope(template.envelope, webAccessHosts)");
    expect(source).not.toContain("requestedEnvelope: template.envelope,");
  });

  test("offers every template host, with no installation list (#680)", () => {
    expect(source).not.toContain("getAdminCapabilities");
    expect(source).toContain("webAccessHostOptions(template.envelope.spec.egress?.hosts ?? [])");
    expect(source).toContain('label="Web access hosts"');
    expect(source).toContain("This template allows no web access.");
  });
});
