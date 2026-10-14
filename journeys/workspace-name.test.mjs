import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

import { defaultWorkspaceName } from "./workspace-name.mjs";

const cases = JSON.parse(readFileSync(new URL("../tests/fixtures/workspace-name-cases.json", import.meta.url), "utf8"));

test("journey workspace paths follow the server workspace-name cases", () => {
  for (const { repositoryName, workspaceName } of cases) {
    expect(defaultWorkspaceName(repositoryName)).toBe(workspaceName ?? "");
  }
});
