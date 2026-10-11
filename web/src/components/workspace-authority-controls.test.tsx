import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";

import type { WorkspaceDeploymentCapabilities } from "@/api-client";
import {
  WorkspaceAuthorityControls,
  formatBinaryBytes,
  formatWorkspaceQuantity,
  workspaceAuthorityDraft,
  workspaceAuthorityFromDraft,
} from "./workspace-authority-controls";

const deployment: WorkspaceDeploymentCapabilities = {
  enabledTypes: ["git", "archive"],
  maxFiles: 1_000,
  maxHistoryDepth: 50,
  maxSubmoduleDepth: 2,
  maxSubmodulesPerEntry: 8,
  maxTotalBytes: 4 * 1024 ** 3,
  maxTotalSubmodules: 16,
  maxTreeReads: 2_000,
  scratchVolumeBytes: 8 * 1024 ** 3,
};

describe("workspace authority controls", () => {
  test("authors only deployment-advertised types and the optional total size", () => {
    const html = renderToStaticMarkup(<WorkspaceAuthorityControls deployment={deployment} draft={{ allowedTypes: ["git"], size: "2", sizeUnit: "GiB" }} onChange={() => {}} />);

    expect(html).toContain("Git");
    expect(html).toContain("Archive");
    expect(html).not.toContain("Scratch");
    expect(html).toContain("Total size limit (optional)");
    expect(html).toContain("deployment limit 4 GiB");
  });

  test("serializes the two-field shape and omits retired operational limits", () => {
    expect(workspaceAuthorityFromDraft({ allowedTypes: ["git"], size: "2", sizeUnit: "GiB" }, deployment.enabledTypes)).toEqual({
      allowedTypes: ["git"],
      maxTotalSize: "2Gi",
    });
    expect(workspaceAuthorityFromDraft({ allowedTypes: ["git"], size: "", sizeUnit: "GiB" }, deployment.enabledTypes)).toEqual({ allowedTypes: ["git"] });
    expect(workspaceAuthorityFromDraft({ allowedTypes: [], size: "2", sizeUnit: "GiB" }, deployment.enabledTypes)).toBeUndefined();
    expect(workspaceAuthorityFromDraft({ allowedTypes: ["git"], size: "0", sizeUnit: "GiB" }, deployment.enabledTypes)).toBeNull();
    expect(workspaceAuthorityFromDraft({ allowedTypes: ["scratch"], size: "", sizeUnit: "GiB" }, deployment.enabledTypes)).toBeUndefined();
  });

  test("preserves legacy narrowing limits while editing the visible fields", () => {
    const draft = workspaceAuthorityDraft({
      allowedTypes: ["git", "scratch"],
      maxFiles: 1_000,
      maxHistoryDepth: 50,
      maxSubmoduleDepth: 2,
      maxTotalSize: "768Mi",
    }, deployment.enabledTypes);

    expect(workspaceAuthorityFromDraft({ ...draft, size: "512" }, deployment.enabledTypes)).toEqual({
      allowedTypes: ["git"],
      maxFiles: 1_000,
      maxHistoryDepth: 50,
      maxSubmoduleDepth: 2,
      maxTotalSize: "512Mi",
    });
  });

  test("formats binary quantities with plain units", () => {
    expect(formatBinaryBytes(2 * 1024 ** 3)).toBe("2 GiB");
    expect(formatWorkspaceQuantity("768Mi")).toBe("768 MiB");
  });
});
