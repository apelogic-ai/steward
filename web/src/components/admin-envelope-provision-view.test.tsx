import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const source = readFileSync(new URL("./admin-envelope-provision-view.tsx", import.meta.url), "utf8");

describe("administrator envelope provisioning", () => {
  test("provisions only the web access hosts the administrator picks, none by default", () => {
    expect(source).toContain("useState<ReadonlySet<string>>(() => new Set())");
    expect(source).toContain("requestedEnvelope: provisionedEnvelope(template.envelope, webAccessHosts)");
    expect(source).not.toContain("requestedEnvelope: template.envelope,");
  });

  test("offers only template hosts this installation still allows", () => {
    expect(source).toContain("getAdminCapabilities");
    expect(source).toContain("allowedWebAccessHosts(template.envelope.spec.egress?.hosts, options.webAccessHosts)");
    expect(source).toContain('label="Web access hosts"');
    expect(source).toContain('kind: "neutral" as const, label: webAccessHostKey(host)');
  });
});
