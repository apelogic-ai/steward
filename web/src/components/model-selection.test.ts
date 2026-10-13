import { describe, expect, test } from "bun:test";

import {
  compatibleAgents,
  effectiveAgentSelection,
  effectiveModelSelection,
  modelOptionLabel,
  preferredTaskModel,
  requirementsWithSelectedModel,
  startingSelection,
} from "./model-selection";

const agents = [
  {
    agentRef: "example-agent@1.0.0",
    displayName: "Example agent",
    models: { providers: ["example", "provider-a"] },
  },
  {
    agentRef: "limited-agent@1.0.0",
    displayName: null,
    models: { providers: ["provider-b"] },
  },
];

describe("configuration-driven model selection", () => {
  test("does not infer compatibility from agent names and keeps every configured model", () => {
    expect(compatibleAgents(agents, [
      { provider: "example", model: "model-x" },
      { provider: "example", model: "model-y" },
      { provider: "provider-a", model: "model-a" },
    ])).toEqual([
      {
        agentRef: "example-agent@1.0.0",
        displayName: "Example agent",
        models: [
          { provider: "example", model: "model-x" },
          { provider: "example", model: "model-y" },
          { provider: "provider-a", model: "model-a" },
        ],
        compatible: true,
        reason: null,
      },
      {
        agentRef: "limited-agent@1.0.0",
        displayName: null,
        models: [],
        compatible: false,
        reason: "This Envelope does not allow a model supported by this coding agent.",
      },
    ]);
  });

  test("falls back to the first compatible agent without assuming a provider family", () => {
    const options = compatibleAgents(agents, [{ provider: "example", model: "model-x" }]);
    expect(effectiveAgentSelection(options, "limited-agent@1.0.0")).toEqual({
      selected: options[0],
      warning: "limited-agent@1.0.0 is not compatible with this Envelope. Selected Example agent; you can choose another.",
    });
  });

  test("starts from deployment configuration, then the Envelope's model order", () => {
    expect(startingSelection(
      agents,
      [
        { provider: "example", model: "model-x" },
        { provider: "provider-a", model: "model-a" },
      ],
      "example-agent@1.0.0",
      { provider: "provider-a", model: "model-a" },
    )).toEqual({
      agent: {
        agentRef: "example-agent@1.0.0",
        displayName: "Example agent",
        models: [
          { provider: "example", model: "model-x" },
          { provider: "provider-a", model: "model-a" },
        ],
        compatible: true,
        reason: null,
      },
      model: { provider: "provider-a", model: "model-a" },
    });

    expect(startingSelection(
      agents,
      [{ provider: "provider-b", model: "model-b" }],
      "missing-agent@1.0.0",
      null,
    )).toEqual({
      agent: {
        agentRef: "limited-agent@1.0.0",
        displayName: null,
        models: [{ provider: "provider-b", model: "model-b" }],
        compatible: true,
        reason: null,
      },
      model: { provider: "provider-b", model: "model-b" },
    });
  });

  test("keeps an exact requested model and explains a fallback", () => {
    const selectedAgent = compatibleAgents([agents[0]], [
      { provider: "example", model: "model-x" },
      { provider: "example", model: "model-y" },
    ])[0];
    const configured = { provider: "example", model: "model-y" };
    expect(effectiveModelSelection(configured, selectedAgent))
      .toEqual({ selected: configured, warning: null });
    expect(effectiveModelSelection(
      { provider: "provider-a", model: "model-a" },
      selectedAgent,
    )).toEqual({
      selected: { provider: "example", model: "model-x" },
      warning: "provider-a/model-a is not available for Example agent in this Envelope. Selected example/model-x; you can choose another.",
    });
  });

  test("keeps a picked model when the UI is showing a compatible agent fallback", () => {
    const selectedAgent = compatibleAgents([agents[0]], [
      { provider: "example", model: "model-x" },
      { provider: "example", model: "model-y" },
    ])[0];
    expect(effectiveModelSelection(
      { provider: "example", model: "model-y" },
      selectedAgent,
    )).toEqual({ selected: { provider: "example", model: "model-y" }, warning: null });
  });

  test("keeps exact identities readable when provider and model names contain slashes", () => {
    const choices = [
      { provider: "provider-a", model: "part/model-a" },
      { provider: "provider-a/part", model: "model-a" },
    ];
    expect(choices.map((model) => modelOptionLabel(model, choices))).toEqual([
      "Provider: provider-a · Model: part/model-a",
      "Provider: provider-a/part · Model: model-a",
    ]);
  });

  test("keeps an edited Task requirement aligned with its selected runtime model", () => {
    const requirements = {
      authority: {
        budget: { monthlyLimit: "10.00", currency: "USD" },
        llms: [{ provider: "example", model: "model-x" }],
        runner: { platforms: [] },
        tools: [],
        ttl: "1h",
      },
    };
    expect(requirementsWithSelectedModel(
      requirements,
      { provider: "example", model: "model-y" },
    )).toEqual({
      authority: {
        ...requirements.authority,
        llms: [{ provider: "example", model: "model-y" }],
      },
    });
  });

  test("keeps the explicit Task runtime model when requirements allow several models", () => {
    expect(preferredTaskModel({
      runtime: { agentRef: "example-agent@1.0.0", model: { provider: "example", model: "runtime-model" } },
      requires: {
        authority: {
          budget: { monthlyLimit: "10.00", currency: "USD" },
          llms: [{ provider: "example", model: "required-model" }],
          runner: { platforms: [] },
          tools: [],
          ttl: "1h",
        },
      },
    })).toEqual({ provider: "example", model: "runtime-model" });
  });
});
