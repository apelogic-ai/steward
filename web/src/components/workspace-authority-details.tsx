import type { WorkspaceAuthority } from "@/api-client";
import { GrantChipList } from "@/components/hs";
import { formatCount } from "@/components/presentation-values";

export function WorkspaceAuthorityDetails({ workspace }: Readonly<{ workspace?: null | WorkspaceAuthority }>) {
  if (!workspace) return <p className="text-sm text-muted-ink">No workspace access</p>;
  return (
    <div aria-label="Workspace access" className="space-y-3" role="group">
      <div>
        <p className="mb-2 text-xs font-semibold text-muted-ink">Allowed types</p>
        <GrantChipList grants={workspace.allowedTypes.map((type) => ({ kind: "neutral", name: type }))} />
      </div>
      <dl className="grid gap-3 text-sm sm:grid-cols-2">
        <div><dt className="text-xs font-semibold text-muted-ink">Total size limit</dt><dd className="mt-1 font-mono">{workspace.maxTotalSize}</dd></div>
        <div><dt className="text-xs font-semibold text-muted-ink">File limit</dt><dd className="mt-1">{formatCount(workspace.maxFiles, "file")}</dd></div>
        <div><dt className="text-xs font-semibold text-muted-ink">History depth limit</dt><dd className="mt-1">{formatCount(workspace.maxHistoryDepth, "commit")}</dd></div>
        <div><dt className="text-xs font-semibold text-muted-ink">Submodule depth limit</dt><dd className="mt-1">{formatCount(workspace.maxSubmoduleDepth, "level")}</dd></div>
      </dl>
    </div>
  );
}
