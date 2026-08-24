import { describe, expect, test } from "bun:test";

import type { SessionResponse } from "@/api-client";

import { classifySessionResult } from "./session-context";

const authenticated: SessionResponse = {
  apiVersion: "steward.browser-session/v1",
  csrf: "test-csrf",
  memberRoles: ["developer"],
  principal: {
    displayEmail: "alice@example.com",
    userId: "usr_abcdef0123456789abcdef0123456789",
  },
  role: "user",
  surfaces: ["envelopeRequests"],
};

describe("session response states", () => {
  test("accepts only a typed successful response as authenticated", () => {
    expect(classifySessionResult({ data: authenticated, response: new Response(null, { status: 200 }) })).toEqual({
      status: "authenticated",
      value: authenticated,
    });
  });

  test("keeps unauthorized, unavailable, and unexpected failures distinct", () => {
    expect(classifySessionResult({ response: new Response(null, { status: 401 }) })).toEqual({ status: "unauthorized" });
    expect(classifySessionResult({ response: new Response(null, { status: 503 }) })).toEqual({ status: "unavailable" });
    expect(classifySessionResult({})).toEqual({ status: "unavailable" });
    expect(classifySessionResult({ response: new Response(null, { status: 403 }) })).toEqual({ status: "error" });
    expect(classifySessionResult({ response: new Response(null, { status: 200 }) })).toEqual({ status: "error" });
  });
});
