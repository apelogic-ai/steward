import { describe, expect, test } from "bun:test";

import { classifyConnectionMutationFailure } from "./connection-mutation-state";

describe("connection mutation diagnostics", () => {
  test("distinguishes staged orchestration from a genuine connection outage", () => {
    expect(classifyConnectionMutationFailure(503, {
      error: "connections.orchestration_not_active",
    })).toBe("orchestration-not-active");
    expect(classifyConnectionMutationFailure(503, {
      error: "connection_broker_unavailable",
    })).toBe("unavailable");
  });

  test("retains the pending OAuth conflict diagnostic", () => {
    expect(classifyConnectionMutationFailure(409, {
      error: "oauth_flow_pending",
    })).toBe("oauth-pending");
  });
});
