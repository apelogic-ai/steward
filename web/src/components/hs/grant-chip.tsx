import { cn } from "@/lib/utils";

export type GrantKind = "model" | "neutral" | "read" | "write" | "destructive";

export function grantKindForAction(action: string): GrantKind {
  const normalized = action.toLowerCase();
  if (normalized.includes("delete") || normalized.includes("admin")) return "destructive";
  if (normalized.includes("write") || normalized.includes("create") || normalized.includes("update")) return "write";
  return "read";
}

const kindClasses: Record<GrantKind, string> = {
  model: "bg-info-soft text-info",
  neutral: "bg-line-soft text-muted-ink",
  read: "bg-ok-soft text-ok",
  write: "bg-warn-soft text-warn",
  destructive: "bg-err-soft text-err",
};

export function GrantChip({ className, kind, name, onRemove }: Readonly<{
  className?: string;
  kind: GrantKind;
  name: string;
  onRemove?: () => void;
}>) {
  return (
    <span className={cn("inline-flex h-6.5 items-center gap-1.5 rounded-full px-2.5", kindClasses[kind], className)}>
      <span className="font-mono text-sm text-ink">{name}</span>
      {kind === "neutral" ? null : <span className="text-[11px] font-semibold">{kind}</span>}
      {onRemove ? (
        <button
          aria-label={`Remove ${name}`}
          className="-me-1 inline-flex size-5 items-center justify-center rounded-full text-muted-ink hover:bg-field hover:text-ink"
          onClick={onRemove}
          type="button"
        >
          ×
        </button>
      ) : null}
    </span>
  );
}

export function GrantChipList({ grants, limit }: Readonly<{
  grants: ReadonlyArray<{ kind: GrantKind; name: string }>;
  limit?: number;
}>) {
  const visible = limit === undefined ? grants : grants.slice(0, limit);
  const remainder = grants.length - visible.length;
  return (
    <div className="flex flex-wrap gap-1.5">
      {visible.map((grant) => <GrantChip key={`${grant.kind}:${grant.name}`} {...grant} />)}
      {remainder > 0 ? <span className="self-center text-[13px] text-muted-ink">+{remainder}</span> : null}
    </div>
  );
}
