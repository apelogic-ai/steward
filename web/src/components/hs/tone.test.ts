import { describe, expect, test } from "bun:test";

import { displayStatus, toneForAdminRequest, toneForEnvelopeRequest, toneForTaskPhase } from "./tone";

describe("Steward status tones", () => {
  test("maps every task phase", () => {
    expect(toneForTaskPhase("succeeded")).toBe("ok");
    expect(toneForTaskPhase("failed")).toBe("err");
    expect(toneForTaskPhase("running")).toBe("info");
    expect(toneForTaskPhase("parked")).toBe("warn");
    expect(toneForTaskPhase("cancelled")).toBe("neutral");
  });

  test("keeps user and admin rejection semantics distinct", () => {
    expect(toneForEnvelopeRequest("rejected")).toBe("err");
    expect(toneForAdminRequest("rejected")).toBe("neutral");
  });

  test("formats server enum labels", () => {
    expect(displayStatus("auto_approved")).toBe("Auto-approved");
    expect(displayStatus("reauth_required")).toBe("reauth required");
  });
});
