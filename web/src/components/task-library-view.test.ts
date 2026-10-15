import { describe, expect, test } from "bun:test";

import { taskSaveFailureMessage } from "./task-library-view";

describe("Task save refusal", () => {
  test("shows the API's plain failure reason", () => {
    expect(taskSaveFailureMessage({
      error: "admission_rejected",
      failureReason: "The per-run budget must be more than 0.00 USD.",
    })).toBe("The per-run budget must be more than 0.00 USD.");
  });

  test("keeps the package guidance when the API supplies no reason", () => {
    expect(taskSaveFailureMessage({ error: "unprocessable_entity" })).toBe(
      "Steward rejected this package. Check its paths, Task definition, skills, workspace, outputs, and requirements.",
    );
    expect(taskSaveFailureMessage({ failureReason: "  " })).toBe(
      "Steward rejected this package. Check its paths, Task definition, skills, workspace, outputs, and requirements.",
    );
  });
});
