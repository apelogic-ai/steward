import type { DirectAdmissionDelta } from "@/api-client";
import { formatDuration, formatMoney } from "@/components/presentation-values";

export type EnvelopeExceedanceSummaryValue = {
  ceiling: string;
  label: string;
  requested: string;
};

function listOrNone(values: Array<string>): string {
  return values.length ? values.join(", ") : "None";
}

function workspace(value: DirectAdmissionDelta & { dimension: "workspace" }, side: "ceiling" | "requested"): string {
  const authority = value[side];
  if (!authority) return "Not set";
  return [
    `types ${listOrNone(authority.allowedTypes)}`,
    `size ${authority.maxTotalSize}`,
    `files ${authority.maxFiles}`,
    `history ${authority.maxHistoryDepth}`,
    `submodules ${authority.maxSubmoduleDepth}`,
  ].join(" · ");
}

export function envelopeDeltaSummary(delta: DirectAdmissionDelta): EnvelopeExceedanceSummaryValue {
  switch (delta.dimension) {
    case "budget":
      return { ceiling: formatMoney(delta.ceiling, delta.currency), label: "Monthly limit", requested: formatMoney(delta.requested, delta.currency) };
    case "singleRunBudget":
      return { ceiling: formatMoney(delta.ceiling, delta.currency), label: "Limit per run", requested: delta.requested ? formatMoney(delta.requested, delta.currency) : "Unbounded" };
    case "runtimeMinutes":
      return { ceiling: formatDuration(delta.ceiling, "minutes"), label: "Runtime limit per month", requested: delta.requested ? formatDuration(delta.requested, "minutes") : "Unbounded" };
    case "ttl":
      return { ceiling: formatDuration(delta.ceiling), label: "Time-to-live limit", requested: formatDuration(delta.requested) };
    case "models":
      return { ceiling: listOrNone(delta.ceiling.map((model) => `${model.provider}/${model.model}`)), label: "Models", requested: listOrNone(delta.requested.map((model) => `${model.provider}/${model.model}`)) };
    case "tools":
      return { ceiling: listOrNone(delta.ceiling.map((tool) => `${tool.provider}:${tool.resource}:${tool.action}`)), label: "Tools", requested: listOrNone(delta.requested.map((tool) => `${tool.provider}:${tool.resource}:${tool.action}`)) };
    case "runnerPlatforms":
      return { ceiling: listOrNone(delta.ceiling), label: "Runner platforms", requested: listOrNone(delta.requested) };
    case "runnerMemory":
      return { ceiling: delta.ceiling ?? "Not set", label: "Runner memory", requested: delta.requested };
    case "runnerCompute":
      return { ceiling: delta.ceiling ?? "Not set", label: "Runner compute", requested: delta.requested };
    case "runnerStorage":
      return { ceiling: delta.ceiling ?? "Not set", label: "Runner storage", requested: delta.requested };
    case "workspace":
      return { ceiling: workspace(delta, "ceiling"), label: "Workspace", requested: workspace(delta, "requested") };
  }
}

export function EnvelopeExceedanceSummary({ values }: Readonly<{ values: Array<EnvelopeExceedanceSummaryValue> }>) {
  if (!values.length) return null;
  return (
    <div className="rounded-control bg-warn-soft p-3 text-sm">
      <strong>Needs admin approval: {values.length} {values.length === 1 ? "value" : "values"} above the template ceiling</strong>
      <ul className="mt-2 list-disc ps-5">{values.map(({ ceiling, label, requested }) => <li key={label}>{label}: <strong>{requested}</strong> (ceiling {ceiling})</li>)}</ul>
    </div>
  );
}
