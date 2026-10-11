import { expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { WorkspaceAuthority } from "@/api-client";
import { WorkspaceAuthorityDetails } from "./workspace-authority-details";

test("workspace types and limits come from the API value", () => {
  const workspace = {
    allowedTypes: ["archive"],
    maxFiles: 42,
    maxHistoryDepth: 12,
    maxSubmoduleDepth: 3,
    maxTotalSize: "768Mi",
  } as unknown as WorkspaceAuthority;

  const html = renderToStaticMarkup(<WorkspaceAuthorityDetails workspace={workspace} />);

  expect(html).toContain("archive");
  expect(html).toContain("768Mi");
  expect(html).toContain("42 files");
  expect(html).toContain("12 commits");
  expect(html).toContain("3 levels");
});

test("an absent workspace is explicit", () => {
  expect(renderToStaticMarkup(<WorkspaceAuthorityDetails workspace={null} />)).toContain("No workspace access");
});
