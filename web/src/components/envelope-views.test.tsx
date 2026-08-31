import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const source = readFileSync(new URL("./envelope-views.tsx", import.meta.url), "utf8");

describe("envelope run history", () => {
  test("filters runs by the durable envelope instance rather than the runtime UID", () => {
    expect(source).toContain("query: { envelopeInstanceId }");
    expect(source).not.toContain("query: { runtimeUid }");
  });
});
