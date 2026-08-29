import { describe, expect, test } from "bun:test";

import { githubConnectionBlocksSelectedTools } from "./envelope-request-state";

describe("GitHub readiness for an envelope request", () => {
  test("blocks only selected tool authority that lacks a ready connection", () => {
    for (const readiness of ["missing", "reauth_required"] as const) {
      expect(githubConnectionBlocksSelectedTools(1, readiness)).toBe(true);
      expect(githubConnectionBlocksSelectedTools(0, readiness)).toBe(false);
    }
    expect(githubConnectionBlocksSelectedTools(1, "connected")).toBe(false);
    expect(githubConnectionBlocksSelectedTools(0, "connected")).toBe(false);
  });
});
