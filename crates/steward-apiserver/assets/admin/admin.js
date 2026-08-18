"use strict";

const tabs = Array.from(document.querySelectorAll("[data-surface]"));
const panels = Array.from(document.querySelectorAll("[data-panel]"));
const operator = document.querySelector(".operator");
const operatorName = document.querySelector("#operator");
const fatal = document.querySelector("#fatal");
const envelopeEditor = document.querySelector("#envelope-editor");
const envelopeLoading = document.querySelector("#envelope-loading");
const envelopeForm = document.querySelector("#envelope-template-form");
const templateEndpoint = "/admin/api/v1/envelope-templates/engineer";
let loadedTemplate = null;

function selectSurface(name) {
  for (const tab of tabs) {
    const active = tab.dataset.surface === name;
    tab.setAttribute("aria-selected", String(active));
    tab.setAttribute("tabindex", active ? "0" : "-1");
    if (active) {
      tab.setAttribute("aria-current", "page");
    } else {
      tab.removeAttribute("aria-current");
    }
  }
  for (const panel of panels) {
    panel.hidden = panel.dataset.panel !== name;
  }
}

for (const tab of tabs) {
  tab.addEventListener("click", () => selectSurface(tab.dataset.surface));
  tab.addEventListener("keydown", (event) => {
    const current = tabs.indexOf(tab);
    let target = null;
    if (event.key === "ArrowLeft") {
      target = tabs[(current - 1 + tabs.length) % tabs.length];
    } else if (event.key === "ArrowRight") {
      target = tabs[(current + 1) % tabs.length];
    } else if (event.key === "Home") {
      target = tabs[0];
    } else if (event.key === "End") {
      target = tabs[tabs.length - 1];
    }
    if (target) {
      event.preventDefault();
      selectSurface(target.dataset.surface);
      window.location.hash = target.dataset.surface;
      target.focus();
    }
  });
}

function selectSurfaceFromHash() {
  const requested = window.location.hash.slice(1) || "approvals";
  if (tabs.some((tab) => tab.dataset.surface === requested)) {
    selectSurface(requested);
  }
}

window.addEventListener("hashchange", selectSurfaceFromHash);
selectSurfaceFromHash();

async function loadBootstrap() {
  try {
    const response = await fetch("/admin/api/v1/bootstrap", {
      headers: { Accept: "application/json" },
      cache: "no-store",
    });
    if (!response.ok) {
      throw new Error("administrator bootstrap rejected");
    }
    const value = await response.json();
    if (
      value.apiVersion !== "steward.admin/v1" ||
      typeof value.actor !== "string" ||
      value.actor.length === 0 ||
      !Array.isArray(value.surfaces) ||
      value.surfaces.length !== tabs.length ||
      value.surfaces.some((surface) =>
        !tabs.some((tab) => tab.dataset.surface === surface)
      )
    ) {
      throw new Error("administrator bootstrap contract mismatch");
    }
    const allowed = new Set(value.surfaces);
    for (const tab of tabs) {
      tab.hidden = !allowed.has(tab.dataset.surface);
    }
    operatorName.textContent = value.actor;
    operator.dataset.ready = "true";
    return true;
  } catch (_error) {
    operatorName.textContent = "Administrator access unavailable";
    operator.dataset.ready = "false";
    fatal.hidden = false;
    return false;
  }
}

function appendChip(container, label, state) {
  const chip = document.createElement("span");
  chip.className = `policy-chip policy-chip-${state}`;
  chip.textContent = label;
  container.append(chip);
}

function renderTemplate(payload) {
  loadedTemplate = payload.template;
  document.querySelector("#template-source").textContent = "Local review fixture";
  document.querySelector("#template-revision").textContent = `Revision ${loadedTemplate.revision}`;
  document.querySelector("#template-name-heading").textContent = loadedTemplate.displayName;
  document.querySelector("#budget-threshold").value = loadedTemplate.thresholds.budgetMonthlyLimit;
  document.querySelector("#budget-ceiling").value = loadedTemplate.envelope.spec.budget.monthlyLimit;
  document.querySelector("#ttl-threshold").value = loadedTemplate.thresholds.ttl;
  document.querySelector("#ttl-ceiling").value = loadedTemplate.envelope.spec.ttl;

  const models = document.querySelector("#model-list");
  models.replaceChildren();
  for (const model of loadedTemplate.envelope.spec.llms) {
    appendChip(models, `${model.provider} / ${model.model}`, "allowed");
  }

  const tools = document.querySelector("#tool-list");
  tools.replaceChildren();
  for (const tool of loadedTemplate.envelope.spec.tools) {
    const row = document.createElement("div");
    row.className = "tool-row";
    const identity = document.createElement("span");
    identity.textContent = `${tool.provider} · ${tool.resource} · ${tool.action}`;
    const authority = document.createElement("strong");
    authority.textContent = "Read · allowed";
    row.append(identity, authority);
    tools.append(row);
  }

  const classes = document.querySelector("#action-class-list");
  classes.replaceChildren();
  for (const actionClass of loadedTemplate.actionClasses) {
    appendChip(
      classes,
      `${actionClass.name} · ${actionClass.state}`,
      actionClass.state === "authoritative" ? "allowed" : "unavailable"
    );
  }

  for (const [name, capability] of Object.entries(loadedTemplate.capabilities)) {
    const explanation = document.querySelector(`[data-capability="${name}"]`);
    if (explanation) {
      explanation.textContent = capability.reason;
    }
  }

  document.querySelector("#proof-base-revision").textContent = String(loadedTemplate.revision);
  envelopeLoading.hidden = true;
  envelopeEditor.hidden = false;
}

async function loadEnvelopeTemplate() {
  try {
    const response = await fetch(templateEndpoint, {
      headers: { Accept: "application/json" },
      cache: "no-store",
    });
    if (!response.ok) {
      throw new Error("envelope template unavailable");
    }
    const payload = await response.json();
    if (
      payload.apiVersion !== "steward.admin/v1" ||
      payload.template?.id !== "engineer" ||
      !Number.isInteger(payload.template.revision) ||
      payload.template.revision !== payload.template.envelope?.revision ||
      !Array.isArray(payload.template.envelope?.spec?.llms) ||
      !Array.isArray(payload.template.envelope?.spec?.tools)
    ) {
      throw new Error("envelope template contract mismatch");
    }
    renderTemplate(payload);
  } catch (_error) {
    envelopeLoading.dataset.state = "error";
    envelopeLoading.querySelector("h2").textContent = "Envelope editor unavailable";
    envelopeLoading.querySelector("p").textContent =
      "The versioned template contract could not be verified. No draft was created.";
  }
}

function resetProof() {
  const card = document.querySelector("#proof-card");
  card.className = "proof-card proof-unknown";
  document.querySelector("#prover-heading").textContent = "Not staged";
  document.querySelector("#prover-summary").textContent =
    "The candidate changed after the last proof. Stage it again before apply.";
  document.querySelector("#proof-candidate-revision").textContent = "—";
  document.querySelector("#affected-agents").textContent = "Unknown";
  document.querySelector("#blast-radius-reason").textContent =
    "No authoritative impact result for the current candidate.";
  document.querySelector("#apply-stage").disabled = true;
}

function buildProofRequest() {
  const candidate = JSON.parse(JSON.stringify(loadedTemplate.envelope));
  candidate.revision = loadedTemplate.revision + 1;
  candidate.spec.budget.monthlyLimit = document.querySelector("#budget-ceiling").value.trim();
  candidate.spec.ttl = document.querySelector("#ttl-ceiling").value.trim();
  return {
    apiVersion: "steward.admin/v1",
    baseRevision: loadedTemplate.revision,
    candidate,
    thresholds: {
      budgetMonthlyLimit: document.querySelector("#budget-threshold").value.trim(),
      ttl: document.querySelector("#ttl-threshold").value.trim(),
    },
  };
}

function renderProof(proof) {
  const card = document.querySelector("#proof-card");
  card.className = `proof-card proof-${proof.verdict}`;
  document.querySelector("#prover-heading").textContent =
    proof.verdict === "unknown" ? "Supported limits valid · impact unknown" : "Conflict";
  document.querySelector("#prover-summary").textContent = proof.reason;
  document.querySelector("#proof-base-revision").textContent = String(proof.baseRevision);
  document.querySelector("#proof-candidate-revision").textContent = String(proof.candidateRevision);
  document.querySelector("#affected-agents").textContent =
    proof.blastRadius.affectedAgents === null ? "Unavailable" : String(proof.blastRadius.affectedAgents);
  document.querySelector("#blast-radius-reason").textContent = proof.blastRadius.reason;
  const propagation = document.querySelector("#propagation-list");
  propagation.replaceChildren();
  for (const target of proof.propagation) {
    const row = document.createElement("li");
    const name = document.createElement("span");
    name.textContent = target.target;
    const state = document.createElement("strong");
    state.textContent = target.state;
    row.append(name, state);
    propagation.append(row);
  }
  document.querySelector("#apply-stage").disabled = !proof.applyAllowed;
}

async function proveEnvelope(event) {
  event.preventDefault();
  if (!loadedTemplate) {
    return;
  }
  const error = document.querySelector("#threshold-error");
  const stageButton = document.querySelector("#stage-template");
  error.hidden = true;
  stageButton.disabled = true;
  stageButton.textContent = "Proving…";
  try {
    const response = await fetch(`${templateEndpoint}/prove`, {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
        "X-Steward-CSRF": "1",
      },
      cache: "no-store",
      body: JSON.stringify(buildProofRequest()),
    });
    const value = await response.json();
    if (!response.ok) {
      throw new Error(
        response.status === 422
          ? "Auto-provision thresholds must be valid and no greater than their hard ceilings."
          : "The candidate could not be proven against the current revision."
      );
    }
    if (
      value.apiVersion !== "steward.admin/v1" ||
      value.baseRevision !== loadedTemplate.revision ||
      value.candidateRevision !== loadedTemplate.revision + 1 ||
      !["safe", "conflict", "unknown"].includes(value.verdict)
    ) {
      throw new Error("The prover returned an invalid revision-bound result.");
    }
    renderProof(value);
  } catch (caught) {
    error.textContent = caught instanceof Error ? caught.message : "The proof failed closed.";
    error.hidden = false;
    resetProof();
  } finally {
    stageButton.disabled = false;
    stageButton.textContent = "Stage and prove";
  }
}

envelopeForm.addEventListener("input", resetProof);
envelopeForm.addEventListener("submit", proveEnvelope);

async function initialize() {
  if (await loadBootstrap()) {
    await loadEnvelopeTemplate();
  }
}

void initialize();
