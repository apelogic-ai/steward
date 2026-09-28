import { cn } from "@/lib/utils";

export function Meter({ label, limit, limitLabel, unit, used, usedLabel }: Readonly<{
  label?: string;
  limit: number;
  limitLabel?: string;
  unit?: string;
  used: number;
  usedLabel?: string;
}>) {
  const percent = limit > 0 ? Math.min(100, Math.max(0, (used / limit) * 100)) : 0;
  const tone = percent >= 100 ? "err" : percent >= 80 ? "warn" : "brand";
  const fillClass = { err: "bg-err", warn: "bg-warn", brand: "bg-brand" }[tone];
  return (
    <div className="space-y-1.5" data-tone={tone}>
      <div className="flex items-baseline justify-between gap-3 text-xs">
        {label ? <span className="font-medium text-muted-ink">{label}</span> : <span />}
        <span className="tabular-nums text-muted-ink"><strong className="font-semibold text-ink">{usedLabel ?? used}</strong> / {limitLabel ?? limit}{unit ? ` ${unit}` : ""}</span>
      </div>
      <div aria-label={label} aria-valuemax={limit} aria-valuemin={0} aria-valuenow={Math.min(used, limit)} className="h-1.5 overflow-hidden rounded-full bg-line-soft" role="meter">
        <div className={cn("h-full rounded-full transition-[width] duration-300", fillClass)} style={{ width: `${percent}%` }} />
      </div>
    </div>
  );
}
