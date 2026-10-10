import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { DirectAdmissionDelta } from "@/api-client";
import { EnvelopeExceedanceSummary, envelopeDeltaSummary } from "./envelope-exceedance-summary";

describe("envelope ceiling-exceedance summaries", () => {
  test("renders one consistent summary for scalar, capability, runner, and workspace deltas", () => {
    const deltas: Array<DirectAdmissionDelta> = [
      { ceiling: "100.00", currency: "USD", dimension: "budget", requested: "125.00" },
      { ceiling: "2.00", currency: "USD", dimension: "singleRunBudget", requested: "5.00" },
      { ceiling: "60", dimension: "runtimeMinutes", requested: "90" },
      { ceiling: "1h", dimension: "ttl", requested: "2h" },
      {
        ceiling: [{ model: "model-a", provider: "provider-a" }],
        dimension: "models",
        requested: [{ model: "model-b", provider: "provider-a" }],
      },
      {
        ceiling: [{ action: "read", provider: "github", resource: "issues_get" }],
        dimension: "tools",
        requested: [{ action: "write", provider: "github", resource: "issues_update" }],
      },
      { ceiling: ["linux"], dimension: "runnerPlatforms", requested: ["linux", "mac"] },
      { ceiling: null, dimension: "runnerMemory", requested: "1Gi" },
      { ceiling: "500m", dimension: "runnerCompute", requested: "1" },
      { ceiling: "5Gi", dimension: "runnerStorage", requested: "10Gi" },
      {
        ceiling: null,
        dimension: "workspace",
        requested: {
          allowedTypes: ["git"],
          maxFiles: 1000,
          maxHistoryDepth: 100,
          maxSubmoduleDepth: 1,
          maxTotalSize: "1Gi",
        },
      },
    ];

    const html = renderToStaticMarkup(<EnvelopeExceedanceSummary values={deltas.map(envelopeDeltaSummary)} />);
    expect(html).toContain("Needs admin approval: 11 values above the template ceiling");
    expect(html).toContain("Monthly limit: <strong>125.00 USD</strong> (ceiling 100.00 USD)");
    expect(html).toContain("Per-run limit: <strong>5.00 USD</strong> (ceiling 2.00 USD)");
    expect(html).toContain("Runtime minutes: <strong>90 minutes</strong> (ceiling 60 minutes)");
    expect(html).toContain("Time to live: <strong>2h</strong> (ceiling 1h)");
    expect(html).toContain("Models: <strong>provider-a/model-b</strong> (ceiling provider-a/model-a)");
    expect(html).toContain("Tools: <strong>github:issues_update:write</strong> (ceiling github:issues_get:read)");
    expect(html).toContain("Runner platforms: <strong>linux, mac</strong> (ceiling linux)");
    expect(html).toContain("Runner memory: <strong>1Gi</strong> (ceiling Not set)");
    expect(html).toContain("Runner compute: <strong>1</strong> (ceiling 500m)");
    expect(html).toContain("Runner storage: <strong>10Gi</strong> (ceiling 5Gi)");
    expect(html).toContain("Workspace: <strong>types git · size 1Gi · files 1000 · history 100 · submodules 1</strong> (ceiling Not set)");
  });
});
