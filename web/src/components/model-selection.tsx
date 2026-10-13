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

export type StartingSelection = {
  agent: CompatibleAgent | undefined;
  model: Model | null;
};

export function modelKey(model: Model): string {
  return JSON.stringify([model.provider, model.model]);
}

export function qualifiedEnvelopeDigest(digest: string): string {
  return digest.startsWith("steward:") ? digest : `steward:${digest}`;
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
    const reason = agent.models.providers.length === 0
      ? "This coding agent does not declare any supported model providers."
      : "This Envelope does not allow a model supported by this coding agent.";
    return {
      agentRef: agent.agentRef,
      displayName: agent.displayName ?? null,
      models,
      compatible: models.length > 0,
      reason: models.length > 0 ? null : reason,
    };
  });
}

export function startingSelection(
  agents: ExecutionBindingAdvertisement[],
  envelopeModels: Model[],
  configuredAgentRef: string,
  configuredModel: Model | null,
): StartingSelection {
  const options = compatibleAgents(agents, envelopeModels);
  const configuredAgent = options.find((agent) => agent.agentRef === configuredAgentRef && agent.compatible);
  const exactConfiguredModel = configuredAgent && configuredModel
    ? configuredAgent.models.find((model) => modelKey(model) === modelKey(configuredModel))
    : undefined;
  if (configuredAgent && exactConfiguredModel) {
    return { agent: configuredAgent, model: exactConfiguredModel };
  }
  if (configuredAgent) {
    return { agent: configuredAgent, model: configuredAgent.models[0] ?? null };
  }
  if (configuredModel) {
    const modelAgent = options.find((agent) => agent.models.some((model) => modelKey(model) === modelKey(configuredModel)));
    if (modelAgent) return { agent: modelAgent, model: configuredModel };
  }
  for (const model of envelopeModels) {
    const modelAgent = options.find((agent) => agent.models.some((candidate) => modelKey(candidate) === modelKey(model)));
    if (modelAgent) return { agent: modelAgent, model };
  }
  return { agent: undefined, model: null };
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
      ? `${requestedAgentRef} is not compatible with this Envelope. Selected ${selected.displayName ?? selected.agentRef}; you can choose another.`
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
      ? `${modelLabel(requestedModel)} is not available for ${selectedAgent.displayName ?? selectedAgent.agentRef} in this Envelope. Selected ${modelLabel(fallback)}; you can choose another.`
      : null,
  };
}

export function preferredTaskModel(
  task: Pick<DirectTaskDefinition, "requires" | "runtime">,
): Model | null {
  return task.runtime.model ?? task.requires?.authority.llms[0] ?? null;
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
  allowNoModel = false,
  className,
  describedBy,
  models,
  onChange,
  preserveUnavailable = false,
  selected,
}: Readonly<{
  allowNoModel?: boolean;
  className: string;
  describedBy?: string;
  models: Model[];
  onChange: (model: Model) => void;
  preserveUnavailable?: boolean;
  selected: Model | null;
}>) {
  const id = useId();
  const selectedIsUnavailable = Boolean(selected && !models.some((model) => modelKey(model) === modelKey(selected)));
  const options = selected && preserveUnavailable && selectedIsUnavailable
    ? [selected, ...models]
    : models;
  return <div className="grid gap-2 text-sm font-semibold">
    <label htmlFor={id}>Model</label>
    <select
      aria-describedby={describedBy}
      className={`${className} font-mono`}
      disabled={options.length === 0 && !allowNoModel}
      id={id}
      onChange={(event) => {
        const model = models.find((candidate) => modelKey(candidate) === event.target.value);
        if (model) onChange(model);
      }}
      value={selected ? modelKey(selected) : ""}
    >
      {allowNoModel ? <option value="">Keep current: no model</option> : options.length === 0 ? <option value="">No compatible model</option> : null}
      {options.map((model) => <option key={modelKey(model)} value={modelKey(model)}>{preserveUnavailable && selected && selectedIsUnavailable && modelKey(model) === modelKey(selected) ? `Keep current: ${modelLabel(model)}` : modelOptionLabel(model, options)}</option>)}
    </select>
  </div>;
}
