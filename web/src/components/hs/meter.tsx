import { cn } from "@/lib/utils";

export function Meter({ label, limit, unit, used }: Readonly<{
  label?: string;
  limit: number;
  unit?: string;
  used: number;
}>) {
  const percent = limit > 0 ? Math.min(100, Math.max(0, (used / limit) * 100)) : 0;
  const tone = percent >= 100 ? "err" : percent >= 80 ? "warn" : "ok";
  const fillClass = { err: "bg-err", warn: "bg-warn", ok: "bg-ok" }[tone];
  return (
    <div className="space-y-1.5" data-tone={tone}>
      <div className="flex items-baseline justify-between gap-3 text-xs">
        {label ? <span className="font-medium text-muted-ink">{label}</span> : <span />}
        <span className="tabular-nums text-muted-ink"><strong className="font-semibold text-ink">{used}</strong> / {limit}{unit ? ` ${unit}` : ""}</span>
      </div>
      <div aria-label={label} aria-valuemax={limit} aria-valuemin={0} aria-valuenow={Math.min(used, limit)} className="h-1.5 overflow-hidden rounded-full bg-line-soft" role="meter">
        <div className={cn("h-full rounded-full", fillClass)} style={{ width: `${percent}%` }} />
      </div>
    </div>
  );
}
