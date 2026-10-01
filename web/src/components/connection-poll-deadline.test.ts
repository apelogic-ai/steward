import { describe, expect, test } from "bun:test";

import { boundedConnectionPollDeadline } from "./connection-poll-deadline";

describe("connection operation polling deadline", () => {
  test("keeps polling briefly after the execution deadline so durable failure is observable", () => {
    const now = Date.parse("2026-10-01T12:00:00Z");
    expect(boundedConnectionPollDeadline("2026-10-01T12:00:40Z", now)).toBe(now + 43_000);
  });

  test("allows an immediate observation window for a reused terminal operation", () => {
    const now = Date.parse("2026-10-01T12:01:00Z");
    expect(boundedConnectionPollDeadline("2026-10-01T12:00:40Z", now)).toBe(now + 3_000);
  });

  test("caps an untrusted server deadline and rejects invalid timestamps", () => {
    const now = Date.parse("2026-10-01T12:00:00Z");
    expect(boundedConnectionPollDeadline("2026-10-01T12:05:00Z", now)).toBe(now + 60_000);
    expect(boundedConnectionPollDeadline("not-a-timestamp", now)).toBeNull();
  });
});
