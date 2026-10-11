import type { WorkspaceAuthority, WorkspaceDeploymentCapabilities } from "@/api-client";

const workspaceSizeUnits = ["KiB", "MiB", "GiB", "TiB"] as const;

type WorkspaceSizeUnit = typeof workspaceSizeUnits[number];
type WorkspaceWireUnit = "Ki" | "Mi" | "Gi" | "Ti";

export type WorkspaceAuthorityDraft = {
  allowedTypes: Array<string>;
  preservedLimits?: Pick<WorkspaceAuthority, "maxFiles" | "maxHistoryDepth" | "maxSubmoduleDepth">;
  size: string;
  sizeUnit: WorkspaceSizeUnit;
};

const wireUnit = {
  KiB: "Ki",
  MiB: "Mi",
  GiB: "Gi",
  TiB: "Ti",
} as const satisfies Record<WorkspaceSizeUnit, string>;

const displayUnit = {
  Ki: "KiB",
  Mi: "MiB",
  Gi: "GiB",
  Ti: "TiB",
} as const satisfies Record<WorkspaceWireUnit, WorkspaceSizeUnit>;

export function formatBinaryBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "Not reported";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"] as const;
  let amount = bytes;
  let index = 0;
  while (amount >= 1024 && index < units.length - 1) {
    amount /= 1024;
    index += 1;
  }
  const precision = Number.isInteger(amount) || amount >= 10 ? 0 : 1;
  return `${amount.toLocaleString("en-US", { maximumFractionDigits: precision })} ${units[index]}`;
}

export function formatWorkspaceQuantity(quantity: string): string {
  const match = /^([1-9][0-9]*)(Ki|Mi|Gi|Ti)$/.exec(quantity);
  return match ? `${Number(match[1]).toLocaleString("en-US")} ${displayUnit[match[2] as WorkspaceWireUnit]}` : quantity;
}

export function workspaceAuthorityDraft(workspace?: null | WorkspaceAuthority, enabledTypes?: ReadonlyArray<string>): WorkspaceAuthorityDraft {
  const match = workspace?.maxTotalSize ? /^([1-9][0-9]*)(Ki|Mi|Gi|Ti)$/.exec(workspace.maxTotalSize) : null;
  const enabled = enabledTypes ? new Set(enabledTypes) : null;
  const preservedLimits = {
    ...(workspace?.maxFiles != null ? { maxFiles: workspace.maxFiles } : {}),
    ...(workspace?.maxHistoryDepth != null ? { maxHistoryDepth: workspace.maxHistoryDepth } : {}),
    ...(workspace?.maxSubmoduleDepth != null ? { maxSubmoduleDepth: workspace.maxSubmoduleDepth } : {}),
  };
  return {
    allowedTypes: workspace?.allowedTypes.filter((type) => !enabled || enabled.has(type)) ?? [],
    ...(Object.keys(preservedLimits).length > 0 ? { preservedLimits } : {}),
    size: match?.[1] ?? "",
    sizeUnit: match ? displayUnit[match[2] as WorkspaceWireUnit] : "GiB",
  };
}

export function workspaceAuthorityFromDraft(draft: WorkspaceAuthorityDraft, enabledTypes: ReadonlyArray<string>): WorkspaceAuthority | null | undefined {
  const enabled = new Set(enabledTypes);
  const allowedTypes = [...new Set(draft.allowedTypes.map((type) => type.trim()).filter((type) => type && enabled.has(type)))];
  if (allowedTypes.length === 0) return undefined;
  const authority = {
    allowedTypes: allowedTypes as WorkspaceAuthority["allowedTypes"],
    ...draft.preservedLimits,
  };
  if (!draft.size.trim()) return authority;
  if (!/^[1-9][0-9]*$/.test(draft.size.trim())) return null;
  return {
    ...authority,
    maxTotalSize: `${draft.size.trim()}${wireUnit[draft.sizeUnit]}`,
  };
}

function workspaceTypeLabel(type: string): string {
  const words = type.replaceAll(/[-_]+/g, " ");
  return words ? `${words[0].toUpperCase()}${words.slice(1)}` : type;
}

export function WorkspaceAuthorityControls({
  deployment,
  disabled = false,
  draft,
  error,
  onChange,
}: Readonly<{
  deployment: WorkspaceDeploymentCapabilities;
  disabled?: boolean;
  draft: WorkspaceAuthorityDraft;
  error?: string;
  onChange: (draft: WorkspaceAuthorityDraft) => void;
}>) {
  const enabledTypes = [...new Set(deployment.enabledTypes)];
  const updateType = (type: string, checked: boolean) => onChange({
    ...draft,
    allowedTypes: checked
      ? [...new Set([...draft.allowedTypes, type])]
      : draft.allowedTypes.filter((candidate) => candidate !== type),
  });
  return (
    <fieldset className="space-y-4">
      <legend className="sr-only">Workspace access</legend>
      <div>
        <p className="mb-2 text-sm font-semibold">Allowed workspace types</p>
        {enabledTypes.length ? <div className="grid gap-3 sm:grid-cols-2">{enabledTypes.map((type) => (
          <label className="flex min-h-11 cursor-pointer items-center gap-2 rounded-control border px-4 text-sm has-[:checked]:border-brand has-[:checked]:bg-brand-soft" key={type}>
            <input checked={draft.allowedTypes.includes(type)} disabled={disabled} onChange={(event) => updateType(type, event.target.checked)} type="checkbox" />
            {workspaceTypeLabel(type)}
          </label>
        ))}</div> : <p className="text-sm text-muted-ink">This installation has no workspace types enabled.</p>}
      </div>
      <label className="grid gap-2 text-sm font-semibold">Total size limit (optional)
        <span className="grid grid-cols-[minmax(0,1fr)_auto] gap-2">
          <input aria-invalid={Boolean(error) || undefined} className="min-h-11 min-w-0 rounded-md border bg-panel px-3 font-normal" disabled={disabled || draft.allowedTypes.length === 0} inputMode="numeric" min={1} onChange={(event) => onChange({ ...draft, size: event.target.value })} placeholder="Deployment limit" step={1} type="number" value={draft.size} />
          <select aria-label="Total size unit" className="min-h-11 rounded-md border bg-panel px-3 font-normal" disabled={disabled || draft.allowedTypes.length === 0} onChange={(event) => onChange({ ...draft, sizeUnit: event.target.value as WorkspaceSizeUnit })} value={draft.sizeUnit}>
            {workspaceSizeUnits.map((unit) => <option key={unit} value={unit}>{unit}</option>)}
          </select>
        </span>
        <span className="text-xs font-normal text-muted-ink">Optional · deployment limit {formatBinaryBytes(deployment.maxTotalBytes)}</span>
        {error ? <span className="text-xs font-normal text-err" role="alert">{error}</span> : null}
      </label>
      <p className="text-xs leading-5 text-muted-ink">Deployment limits also apply: {deployment.maxFiles.toLocaleString("en-US")} files, {deployment.maxHistoryDepth.toLocaleString("en-US")} generations, and {deployment.maxSubmoduleDepth.toLocaleString("en-US")} submodule levels.</p>
    </fieldset>
  );
}
