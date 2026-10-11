import type { InputHTMLAttributes } from "react";

type SharedInputProps = Omit<InputHTMLAttributes<HTMLInputElement>, "onChange" | "type" | "value"> & {
  onChange: (value: string) => void;
  value: string;
};

type DurationUnit = "s" | "m" | "h" | "d";

const durationUnits: ReadonlyArray<{ label: string; value: DurationUnit }> = [
  { label: "s", value: "s" },
  { label: "min", value: "m" },
  { label: "h", value: "h" },
  { label: "d", value: "d" },
];

function decimalWithTwoPlaces(value: string | number): string {
  const source = String(value).trim();
  const match = /^([+-]?)([0-9]+)(?:\.([0-9]*))?$/.exec(source);
  if (!match) return source;
  const negative = match[1] === "-";
  const whole = BigInt(match[2]);
  const fraction = `${match[3] ?? ""}000`;
  let hundredths = whole * 100n + BigInt(fraction.slice(0, 2));
  if (fraction[2] >= "5") hundredths += 1n;
  const formattedWhole = hundredths / 100n;
  const formattedFraction = String(hundredths % 100n).padStart(2, "0");
  return `${negative && hundredths !== 0n ? "-" : ""}${formattedWhole}.${formattedFraction}`;
}

export function formatMoney(value: string | number, currency: string): string {
  return `${decimalWithTwoPlaces(value)} ${currency}`;
}

function durationSeconds(value: string): bigint | null {
  const match = /^([0-9]+)(s|m|h|d)$/.exec(value);
  if (!match) return null;
  const multiplier = { s: 1n, m: 60n, h: 3600n, d: 86400n }[match[2] as DurationUnit];
  return BigInt(match[1]) * multiplier;
}

function formatSeconds(totalSeconds: bigint): string {
  const units = [
    [86400n, "d"],
    [3600n, "h"],
    [60n, "min"],
    [1n, "s"],
  ] as const;
  let remaining = totalSeconds;
  const parts: Array<string> = [];
  for (const [size, label] of units) {
    const amount = remaining / size;
    if (amount > 0n) {
      parts.push(`${amount} ${label}`);
      remaining %= size;
    }
  }
  return parts.join(" ") || "0 s";
}

export function formatDuration(value: string, source: "wire" | "minutes" = "wire"): string {
  if (source === "minutes") {
    if (!/^[0-9]+$/.test(value)) return value ? `${value} min` : value;
    return formatSeconds(BigInt(value) * 60n);
  }
  const seconds = durationSeconds(value);
  return seconds === null ? value : formatSeconds(seconds);
}

export function formatCount(value: number, noun: string, plural = `${noun}s`): string {
  return `${value} ${value === 1 ? noun : plural}`;
}

export function MoneyInput({ className = "", currency, onChange, ...props }: SharedInputProps & { currency: string }) {
  return (
    <span className={`flex min-h-11 min-w-0 items-center overflow-hidden rounded-control border bg-panel ${className}`}>
      <input {...props} className="min-w-0 flex-1 bg-transparent px-3 font-normal outline-none" min="0" onChange={(event) => onChange(event.target.value)} step="any" type="number" />
      <span aria-hidden="true" className="border-s px-3 text-sm font-normal text-muted-ink">{currency || "currency"}</span>
    </span>
  );
}

export function DurationInput({ className = "", mode = "wire", onChange, unitAriaLabel = "Duration unit", value, ...props }: SharedInputProps & { mode?: "wire" | "minutes"; unitAriaLabel?: string }) {
  if (mode === "minutes") {
    return (
      <span className={`flex min-h-11 min-w-0 items-center overflow-hidden rounded-control border bg-panel ${className}`}>
        <input {...props} className="min-w-0 flex-1 bg-transparent px-3 font-normal outline-none" min="0" onChange={(event) => onChange(event.target.value)} step="any" type="number" value={value} />
        <span aria-hidden="true" className="border-s px-3 text-sm font-normal text-muted-ink">min</span>
      </span>
    );
  }
  const match = /^([0-9]+)(s|m|h|d)$/.exec(value);
  const amount = match?.[1] ?? value.replace(/[^0-9]/g, "");
  const unit = (match?.[2] as DurationUnit | undefined) ?? "m";
  return (
    <span className={`flex min-h-11 min-w-0 items-center overflow-hidden rounded-control border bg-panel ${className}`}>
      <input {...props} className="min-w-0 flex-1 bg-transparent px-3 font-normal outline-none" min="0" onChange={(event) => onChange(event.target.value ? `${event.target.value}${unit}` : "")} type="number" value={amount} />
      <select aria-label={unitAriaLabel} className="min-h-11 border-s bg-panel px-2 text-sm font-normal" disabled={props.disabled} onChange={(event) => onChange(amount ? `${amount}${event.target.value}` : "")} value={unit}>
        {durationUnits.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}
      </select>
    </span>
  );
}
