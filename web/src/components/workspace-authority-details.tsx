import type { WorkspaceAuthority, WorkspaceDeploymentCapabilities } from "@/api-client";
import { GrantChipList } from "@/components/hs";
import { formatBinaryBytes, formatWorkspaceQuantity } from "@/components/workspace-authority-controls";

function limitValue<T>(value: null | T | undefined, deployment: T | undefined, format: (value: T) => string, showDeploymentFallback: boolean): string | null {
  if (value !== null && value !== undefined) return format(value);
  if (deployment !== undefined) return `Deployment limit · ${format(deployment)}`;
  return showDeploymentFallback ? "Deployment limit" : null;
}

function countLabel(value: number, singular: string): string {
  return `${value.toLocaleString("en-US")} ${value === 1 ? singular : `${singular}s`}`;
}

export function WorkspaceAuthorityDetails({ deployment, showDeploymentFallback = false, workspace }: Readonly<{ deployment?: WorkspaceDeploymentCapabilities; showDeploymentFallback?: boolean; workspace?: null | WorkspaceAuthority }>) {
  if (!workspace) return <p className="text-sm text-muted-ink">No workspace access</p>;
  const limits = [
    ["Total size limit", limitValue(workspace.maxTotalSize, deployment ? String(deployment.maxTotalBytes) : undefined, (value) => /^\d+$/.test(value) ? formatBinaryBytes(Number(value)) : formatWorkspaceQuantity(value), showDeploymentFallback)],
    ["File limit", limitValue(workspace.maxFiles, deployment?.maxFiles, (value) => countLabel(value, "file"), showDeploymentFallback)],
    ["History depth", limitValue(workspace.maxHistoryDepth, deployment?.maxHistoryDepth, (value) => countLabel(value, "generation"), showDeploymentFallback)],
    ["Submodule depth", limitValue(workspace.maxSubmoduleDepth, deployment?.maxSubmoduleDepth, (value) => countLabel(value, "level"), showDeploymentFallback)],
  ].filter((entry): entry is [string, string] => entry[1] !== null);
  return (
    <div aria-label="Workspace access" className="space-y-3" role="group">
      <div>
        <p className="mb-2 text-xs font-semibold text-muted-ink">Allowed types</p>
        <GrantChipList grants={workspace.allowedTypes.map((type) => ({ kind: "neutral", name: type }))} />
      </div>
      {limits.length ? <dl className="grid gap-3 text-sm sm:grid-cols-2">{limits.map(([label, value]) => <div key={label}><dt className="text-xs font-semibold text-muted-ink">{label}</dt><dd className="mt-1">{value}</dd></div>)}</dl> : null}
    </div>
  );
}
