import { expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { WorkspaceAuthority, WorkspaceDeploymentCapabilities } from "@/api-client";
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
  expect(html).toContain("768 MiB");
  expect(html).toContain("42 files");
  expect(html).toContain("12 generations");
  expect(html).toContain("3 levels");
});

test("an absent workspace is explicit", () => {
  expect(renderToStaticMarkup(<WorkspaceAuthorityDetails workspace={null} />)).toContain("No workspace access");
});

test("two-field workspace authority shows known deployment limits without undefined", () => {
  const workspace: WorkspaceAuthority = { allowedTypes: ["git"], maxTotalSize: "2Gi" };
  const deployment: WorkspaceDeploymentCapabilities = {
    enabledTypes: ["git", "scratch"],
    maxFiles: 1_000,
    maxHistoryDepth: 50,
    maxSubmoduleDepth: 2,
    maxSubmodulesPerEntry: 8,
    maxTotalBytes: 4 * 1024 ** 3,
    maxTotalSubmodules: 16,
    maxTreeReads: 2_000,
    scratchVolumeBytes: 8 * 1024 ** 3,
  };

  const html = renderToStaticMarkup(<WorkspaceAuthorityDetails deployment={deployment} workspace={workspace} />);

  expect(html).toContain("2 GiB");
  expect(html).toContain("Deployment limit · 1,000 files");
  expect(html).toContain("Deployment limit · 50 generations");
  expect(html).toContain("Deployment limit · 2 levels");
  expect(html).not.toContain("undefined");
});

test("omitted limits are left out when deployment values are unknown", () => {
  const html = renderToStaticMarkup(<WorkspaceAuthorityDetails workspace={{ allowedTypes: ["git"] }} />);

  expect(html).toContain("git");
  expect(html).not.toContain("Deployment limit");
  expect(html).not.toContain("undefined");
});
