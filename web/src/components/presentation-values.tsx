import { useEffect, useId, useRef, useState, type InputHTMLAttributes } from "react";

type SharedInputProps = Omit<InputHTMLAttributes<HTMLInputElement>, "onChange" | "type" | "value"> & {
  onChange: (value: string) => void;
  value: string;
};

export type DurationUnit = "s" | "m" | "h" | "d";

const durationUnits: ReadonlyArray<{ label: string; value: DurationUnit }> = [
  { label: "s", value: "s" },
  { label: "min", value: "m" },
  { label: "h", value: "h" },
  { label: "d", value: "d" },
];

function displayDecimal(value: string | number): string | null {
  const source = String(value).trim();
  const match = /^([+-]?)(?:(\d+)(?:\.(\d*))?|\.(\d+))$/.exec(source);
  if (!match) return null;
  const negative = match[1] === "-";
  const whole = BigInt(match[2] ?? "0");
  const fraction = (match[3] ?? match[4] ?? "").padEnd(2, "0");
  const isZero = whole === 0n && !/[1-9]/.test(fraction);
  return `${negative && !isZero ? "-" : ""}${whole}.${fraction}`;
}

export function formatMoney(value: string | number, currency: string): string {
  const source = String(value).trim();
  const unit = currency.trim();
  if (!source || !unit) return "Not set";
  const decimal = displayDecimal(source);
  return decimal === null ? "—" : `${decimal} ${unit}`;
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

function formatSecondsInUnit(totalSeconds: bigint, unit: DurationUnit): string {
  const size = { s: 1n, m: 60n, h: 3600n, d: 86400n }[unit];
  const whole = totalSeconds / size;
  const remainder = totalSeconds % size;
  let amount = String(whole);
  if (remainder) {
    let current = remainder;
    let fraction = "";
    for (let index = 0; index < 6 && current; index += 1) {
      current *= 10n;
      fraction += String(current / size);
      current %= size;
    }
    amount += `.${fraction.replace(/0+$/, "")}`;
  }
  return `${amount} ${unit === "m" ? "min" : unit}`;
}

export function durationUnit(value: string): DurationUnit {
  return (/([smhd])$/.exec(value)?.[1] as DurationUnit | undefined) ?? "m";
}

export function formatDuration(value: string, source: "wire" | "minutes" = "wire", displayUnit?: DurationUnit): string {
  let seconds: bigint | null;
  if (source === "minutes") {
    if (!/^[0-9]+$/.test(value)) return value ? `${value} min` : value;
    seconds = BigInt(value) * 60n;
  } else {
    seconds = durationSeconds(value);
  }
  if (seconds === null) return value;
  return displayUnit ? formatSecondsInUnit(seconds, displayUnit) : formatSeconds(seconds);
}

export function formatCount(value: number, noun: string, plural = `${noun}s`): string {
  return `${value} ${value === 1 ? noun : plural}`;
}

export function MoneyInput({ className = "", currency, onChange, ...props }: SharedInputProps & { currency: string }) {
  const unitId = useId();
  const describedBy = [props["aria-describedby"], unitId].filter(Boolean).join(" ");
  return (
    <span className={`flex min-h-11 min-w-0 items-center overflow-hidden rounded-control border bg-panel focus-within:outline focus-within:outline-2 focus-within:outline-offset-2 focus-within:outline-brand ${className}`}>
      <input {...props} aria-describedby={describedBy} className="min-w-0 flex-1 bg-transparent px-3 font-normal outline-none" min="0" onChange={(event) => onChange(event.target.value)} step="any" type="number" />
      <span className="border-s px-3 text-sm font-normal text-muted-ink" id={unitId}>{currency || "currency"}</span>
    </span>
  );
}

export function DurationInput({ className = "", mode = "wire", onChange, unitAriaLabel = "Duration unit", value, ...props }: SharedInputProps & { mode?: "wire" | "minutes"; unitAriaLabel?: string }) {
  const unitId = useId();
  const unitDescriptionId = useId();
  const errorId = useId();
  const describedBy = [props["aria-describedby"], mode === "wire" ? unitDescriptionId : unitId].filter(Boolean).join(" ");
  const initial = /^([0-9]+)(s|m|h|d)$/.exec(value);
  const [amount, setAmount] = useState(initial?.[1] ?? value.replace(/[smhd]$/, ""));
  const [unit, setUnit] = useState<DurationUnit>((initial?.[2] as DurationUnit | undefined) ?? durationUnit(value));
  const inputRef = useRef<HTMLInputElement>(null);
  const invalid = amount !== "" && !/^\d+$/.test(amount);
  useEffect(() => {
    inputRef.current?.setCustomValidity(invalid ? "Enter a whole number." : "");
  }, [invalid]);
  const emit = (nextAmount: string, nextUnit: DurationUnit) => {
    const nextValue = nextAmount && /^\d+$/.test(nextAmount) ? `${nextAmount}${nextUnit}` : "";
    onChange(nextValue);
  };
  if (mode === "minutes") {
    return (
      <span className={`flex min-h-11 min-w-0 items-center overflow-hidden rounded-control border bg-panel focus-within:outline focus-within:outline-2 focus-within:outline-offset-2 focus-within:outline-brand ${className}`}>
        <input {...props} aria-describedby={describedBy} className="min-w-0 flex-1 bg-transparent px-3 font-normal outline-none" min="0" onChange={(event) => onChange(event.target.value)} step="any" type="number" value={value} />
        <span className="border-s px-3 text-sm font-normal text-muted-ink" id={unitId}>min</span>
      </span>
    );
  }
  return (
    <span className="grid gap-1">
      <span className={`flex min-h-11 min-w-0 items-center overflow-hidden rounded-control border bg-panel focus-within:outline focus-within:outline-2 focus-within:outline-offset-2 focus-within:outline-brand ${className}`}>
        <input {...props} aria-describedby={[describedBy, invalid ? errorId : null].filter(Boolean).join(" ")} aria-invalid={invalid || props["aria-invalid"] || undefined} className="min-w-0 flex-1 bg-transparent px-3 font-normal outline-none" min="0" onChange={(event) => { setAmount(event.target.value); emit(event.target.value, unit); }} ref={inputRef} step="1" type="number" value={amount} />
        <select aria-label={unitAriaLabel} className="min-h-11 border-s bg-panel px-2 text-sm font-normal" disabled={props.disabled} onChange={(event) => { const nextUnit = event.target.value as DurationUnit; setUnit(nextUnit); emit(amount, nextUnit); }} value={unit}>
          {durationUnits.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}
        </select>
        <span className="sr-only" id={unitDescriptionId}>{durationUnits.find((option) => option.value === unit)?.label}</span>
      </span>
      {invalid ? <span className="text-xs font-normal text-err" id={errorId} role="alert">Enter a whole number.</span> : null}
    </span>
  );
}
