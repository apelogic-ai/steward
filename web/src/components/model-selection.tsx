import { useId } from "react";

import type {
  DirectTaskDefinition,
  ExecutionBindingAdvertisement,
  ModelRequirement,
} from "@/api-client";

export type Model = Pick<ModelRequirement, "model" | "provider">;

export type CompatibleAgent = {
  agentRef: string;
  displayName: string | null;
  models: Model[];
  compatible: boolean;
  reason: string | null;
};

export type EffectiveAgentSelection = {
  selected: CompatibleAgent | undefined;
  warning: string | null;
};

export type EffectiveModelSelection = {
  selected: Model | null;
  warning: string | null;
};

export function modelKey(model: Model): string {
  return JSON.stringify([model.provider, model.model]);
}

export function modelLabel(model: Model): string {
  return `${model.provider}/${model.model}`;
}

export function modelOptionLabel(model: Model, choices: Model[]): string {
  const display = modelLabel(model);
  return choices.some((other) => modelKey(other) !== modelKey(model) && modelLabel(other) === display)
    ? `Provider: ${model.provider} · Model: ${model.model}`
    : display;
}

export function compatibleAgents(
  agents: ExecutionBindingAdvertisement[],
  envelopeModels: Model[],
): CompatibleAgent[] {
  return agents.map((agent) => {
    const providers = new Set(agent.models.providers);
    const models = envelopeModels.filter((model) => providers.has(model.provider));
    return {
      agentRef: agent.agentRef,
      displayName: agent.displayName ?? null,
      models,
      compatible: models.length > 0,
      reason: models.length > 0
        ? null
        : "This Envelope does not allow a model supported by this coding agent.",
    };
  });
}

export function effectiveAgentSelection(
  agentOptions: CompatibleAgent[],
  requestedAgentRef: string,
): EffectiveAgentSelection {
  const requested = agentOptions.find((agent) => agent.agentRef === requestedAgentRef);
  const selected = requested?.compatible
    ? requested
    : agentOptions.find((agent) => agent.compatible);
  return {
    selected,
    warning: selected && selected.agentRef !== requestedAgentRef
      ? `${requestedAgentRef} is not compatible with this Envelope. Using ${selected.agentRef} instead.`
      : selected
        ? null
        : requested?.reason ?? "No compatible coding agent is available for this Envelope.",
  };
}

export function effectiveModelSelection(
  requestedModel: Model | null,
  selectedAgent: CompatibleAgent | undefined,
): EffectiveModelSelection {
  if (!selectedAgent?.compatible) return { selected: null, warning: null };
  const fallback = selectedAgent.models[0] ?? null;
  if (!requestedModel) {
    return { selected: fallback, warning: null };
  }
  const exact = selectedAgent.models.find((model) => modelKey(model) === modelKey(requestedModel));
  if (exact) return { selected: exact, warning: null };
  return {
    selected: fallback,
    warning: fallback
      ? `${modelLabel(requestedModel)} is not available for ${selectedAgent.agentRef} in this Envelope. Using ${modelLabel(fallback)} instead.`
      : null,
  };
}

export function preferredTaskModel(
  task: Pick<DirectTaskDefinition, "requires" | "runtime">,
): Model | null {
  return task.requires?.authority.llms[0] ?? task.runtime.model ?? null;
}

export function requirementsWithSelectedModel(requirements: unknown, model: Model): Record<string, unknown> | null {
  if (!requirements || typeof requirements !== "object" || Array.isArray(requirements)) return null;
  const authority = (requirements as Record<string, unknown>).authority;
  if (!authority || typeof authority !== "object" || Array.isArray(authority)) return null;
  return {
    ...(requirements as Record<string, unknown>),
    authority: {
      ...(authority as Record<string, unknown>),
      llms: [model],
    },
  };
}

export function ModelPicker({
  className,
  models,
  onChange,
  selected,
}: Readonly<{
  className: string;
  models: Model[];
  onChange: (model: Model) => void;
  selected: Model | null;
}>) {
  const id = useId();
  return <div className="grid gap-2 text-sm font-semibold">
    <label htmlFor={id}>Model</label>
    <select
      className={`${className} font-mono`}
      disabled={models.length === 0}
      id={id}
      onChange={(event) => {
        const model = models.find((candidate) => modelKey(candidate) === event.target.value);
        if (model) onChange(model);
      }}
      value={selected ? modelKey(selected) : ""}
    >
      {models.length === 0 ? <option value="">No compatible model</option> : null}
      {models.map((model) => <option key={modelKey(model)} value={modelKey(model)}>{modelOptionLabel(model, models)}</option>)}
    </select>
  </div>;
}
