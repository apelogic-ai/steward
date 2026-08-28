import { describe, expect, test } from "bun:test";

import { initialEnvelopeTemplate } from "./admin-template-view";

describe("first envelope template", () => {
  test("starts as an editable, least-authority version-one template", () => {
    expect(initialEnvelopeTemplate).toEqual({
      revision: 1,
      spec: {
        budget: { currency: "USD", monthlyLimit: "0.10", singleRunLimit: "0.10" },
        llms: [{ provider: "openai", model: "gpt-5.4" }],
        tools: [],
        ttl: "15m",
        runner: { platforms: ["linux"] },
      },
    });
  });
});
