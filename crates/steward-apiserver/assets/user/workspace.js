"use strict";

const API = "/app/api/v1";
const ACCORDION_PREFIX = "steward.ui.envelope-accordion.";
const ACCORDIONS = new Set(["templates", "drafts", "approved", "in-review"]);
const path = window.location.pathname;
const pages = Array.from(document.querySelectorAll("[data-page]"));

function activePage() {
  if (path === "/admin/workspace") return "/admin/workspace";
  if (/^\/runs\/[0-9a-f-]{36}$/i.test(path)) return "/runs/detail";
  if (/^\/envelopes\/[^/]+\/runs$/i.test(path)) return "/envelopes/runs";
  if (/^\/envelopes\/[^/]+$/i.test(path)) return path.endsWith("/new") ? "/envelopes/new" : "/envelopes/detail";
  if (path === "/app/envelopes") return "/envelopes";
  if (path === "/app/envelopes/new") return "/envelopes/new";
  if (/^\/app\/envelopes\/[0-9a-f-]{36}$/i.test(path)) return "/envelopes/detail";
  return path;
}

const PAGE_TITLES = {
  "/envelopes": "Envelopes · Steward",
  "/envelopes/new": "New envelope · Steward",
  "/envelopes/detail": "Envelope · Steward",
  "/envelopes/runs": "Envelope runs · Steward",
  "/runs": "Runs · Steward",
  "/runs/detail": "Run · Steward",
  "/admin/workspace": "Admin · Steward",
  "/settings": "Settings · Steward",
};
document.title = PAGE_TITLES[activePage()] ?? "Steward";

for (const page of pages) page.hidden = page.dataset.page !== activePage();
function primaryNavigationRoute() {
  const page = activePage();
  if (page.startsWith("/envelopes")) return "/envelopes";
  if (page.startsWith("/runs")) return "/runs";
  return page;
}
for (const link of document.querySelectorAll("[data-route]")) {
  if (link.dataset.route === primaryNavigationRoute()) link.setAttribute("aria-current", "page");
}

function text(element, value) { element.textContent = value; }
function create(tag, value) { const element = document.createElement(tag); if (value) text(element, value); return element; }
function requestId() { return path.split("/").filter(Boolean).at(-1); }
function envelopeIdForRuns() { return path.split("/").filter(Boolean).at(-2); }
function runId() { return path.split("/").filter(Boolean).at(-1); }

function setupAccordions() {
  for (const details of document.querySelectorAll("details[data-accordion]")) {
    const name = details.dataset.accordion;
    if (!ACCORDIONS.has(name)) continue;
    try {
      if (localStorage.getItem(`${ACCORDION_PREFIX}${name}`) === "open") details.open = true;
    } catch (_) { /* Preferences are optional. */ }
    details.addEventListener("toggle", () => {
      try { localStorage.setItem(`${ACCORDION_PREFIX}${name}`, details.open ? "open" : "closed"); } catch (_) { /* Preferences are optional. */ }
    });
  }
}

async function api(pathname, options) {
  const response = await fetch(`${API}${pathname}`, options);
  if (!response.ok) throw new Error(`The authoritative request failed (${response.status}).`);
  return response.json();
}

async function loadIdentity() {
  const target = document.querySelector("#signed-in-email");
  try {
    const session = await sessionInfo();
    text(target, session.principal.displayEmail);
    setupWorkspaceMode(session);
    const canonicalUserId = document.querySelector("#canonical-user-id");
    if (canonicalUserId) text(canonicalUserId, session.principal.userId);
  } catch (_) { text(target, "Session unavailable"); }
}

function setupWorkspaceMode(value) {
  const control = document.querySelector("#workspace-mode");
  const select = document.querySelector("#workspace-mode-select");
  const dualRole = value.role === "admin"
    && Array.isArray(value.memberRoles)
    && value.memberRoles.includes("developer");
  control.hidden = !dualRole;
  if (!dualRole) return;
  select.value = activePage() === "/admin/workspace" ? "admin" : "developer";
  select.addEventListener("change", () => {
    if (select.value === "admin") window.location.assign("/admin/workspace");
    else window.location.assign("/envelopes");
  });
}

let sessionPromise;
async function sessionInfo() {
  if (!sessionPromise) {
    sessionPromise = fetch("/admin/api/v1/session").then((response) => {
      if (!response.ok) throw new Error("session unavailable");
      return response.json();
    });
  }
  return sessionPromise;
}

async function runsApi(suffix = "", query = {}) {
  const session = await sessionInfo();
  const endpoint = activePage() === "/admin/workspace" && session.role === "admin"
    ? "/admin/api/v1/all-runs"
    : `${API}/runs`;
  const params = new URLSearchParams(Object.entries(query).filter(([, value]) => value !== null && value !== undefined && value !== ""));
  const response = await fetch(`${endpoint}${suffix}${params.size ? `?${params}` : ""}`);
  if (!response.ok) throw new Error(`The authoritative run request failed (${response.status}).`);
  return response.json();
}

function renderStatus(request) {
  const badge = create("span", request.status);
  badge.className = `status status-${request.status}`;
  return badge;
}

function renderRequestSummary(request) {
  const card = create("article");
  card.className = "card";
  const heading = create("h2");
  const link = create("a", `${request.templateId} · revision ${request.templateRevision}`);
  link.href = `/envelopes/${request.id}`;
  heading.append(link);
  card.append(heading, renderStatus(request));
  card.append(create("p", `Requested ${request.createdAt}. Current status recorded ${request.statusAt}.`));
  return card;
}

function renderTemplateSummary(template) {
  const card = create("article");
  card.className = "card";
  const heading = create("h2", `${template.displayName} · revision ${template.revision}`);
  const start = create("a", "Request from this template");
  start.href = `/envelopes/new?template=${encodeURIComponent(template.id)}`;
  start.className = "button";
  card.append(heading, create("p", `Hard budget ceiling: ${template.ceiling.spec.budget.monthlyLimit} ${template.ceiling.spec.budget.currency}.`), start);
  return card;
}

function renderGroup(target, records, empty) {
  target.replaceChildren(...(records.length ? records.map(renderRequestSummary) : [create("p", empty)]));
}

async function loadEnvelopeGroups() {
  const message = document.querySelector("#envelope-list-message");
  const templates = document.querySelector("#template-list");
  const drafts = document.querySelector("#draft-list");
  const approved = document.querySelector("#approved-list");
  const review = document.querySelector("#review-list");
  try {
    const [templateResponse, requestResponse] = await Promise.all([
      api("/envelope-templates"), api("/envelope-requests"),
    ]);
    templates.replaceChildren(...(templateResponse.templates.length ? templateResponse.templates.map(renderTemplateSummary) : [create("p", "No entries.")]));
    text(drafts, "No entries.");
    const records = requestResponse.requests;
    renderGroup(approved, records.filter((record) => ["approved", "provisioned"].includes(record.status)), "No entries.");
    renderGroup(review, records.filter((record) => !["approved", "provisioned"].includes(record.status)), "No entries.");
    text(message, "");
  } catch (error) { text(message, error.message); }
}

function selectedTemplate(templates, select) { return templates.find((template) => template.id === select.value); }
function decimal(value) { const parsed = Number(value); return Number.isFinite(parsed) ? parsed : null; }
function templateBaseline(template) { return template.autoProvisionThreshold || template.ceiling; }
function distinct(values) { return [...new Set(values.filter((value) => value))]; }
function modelLabel(model) { return `${model.provider}/${model.model}`; }
function toolLabel(tool) { return `${tool.provider}:${tool.resource}:${tool.action}`; }

function setChoiceOptions(select, values, selectedValues) {
  const selected = new Set(selectedValues);
  select.replaceChildren(...values.map((value) => {
    const option = create("option", value);
    option.value = value;
    option.selected = selected.has(value);
    return option;
  }));
}

function selectedChoiceValues(select) {
  return Array.from(select.selectedOptions).map((option) => option.value);
}

function renderTemplate(template) {
  const summary = document.querySelector("#template-summary");
  summary.replaceChildren();
  summary.append(create("strong", `${template.displayName} · revision ${template.revision}`));
  summary.append(create("p", "Select only authority and capacity bounded by this template."));
  const hardRunner = template.ceiling.spec.runner || {};
  const baseline = templateBaseline(template);
  setChoiceOptions(
    document.querySelector("#model-select"),
    template.ceiling.spec.llms.map(modelLabel),
    baseline.spec.llms.map(modelLabel),
  );
  setChoiceOptions(
    document.querySelector("#tool-select"),
    template.ceiling.spec.tools.map(toolLabel),
    baseline.spec.tools.map(toolLabel),
  );
  setChoiceOptions(
    document.querySelector("#connection-select"),
    [`GitHub · ${template.githubConnection}`],
    [`GitHub · ${template.githubConnection}`],
  );
  document.querySelector("#budget-limit").value = baseline.spec.budget.monthlyLimit;
  document.querySelector("#ttl").value = baseline.spec.ttl;
  const thresholdRunner = baseline.spec.runner || {};
  const platform = document.querySelector("#runner-platform");
  const platforms = hardRunner.platforms || [];
  setChoiceOptions(platform, platforms, [thresholdRunner.platforms?.[0] || platforms[0] || ""]);
  setChoiceOptions(document.querySelector("#runner-memory"), distinct([thresholdRunner.memory, hardRunner.memory]), [thresholdRunner.memory || hardRunner.memory || ""]);
  setChoiceOptions(document.querySelector("#runner-compute"), distinct([thresholdRunner.compute, hardRunner.compute]), [thresholdRunner.compute || hardRunner.compute || ""]);
  setChoiceOptions(document.querySelector("#runner-storage"), distinct([thresholdRunner.storage, hardRunner.storage]), [thresholdRunner.storage || hardRunner.storage || ""]);
  renderDelta(template);
}

function renderDelta(template) {
  const budget = decimal(document.querySelector("#budget-limit").value);
  const ceiling = decimal(template.ceiling.spec.budget.monthlyLimit);
  const threshold = decimal(template.autoProvisionThreshold?.spec.budget.monthlyLimit);
  const ttl = document.querySelector("#ttl").value;
  const thresholdRunner = templateBaseline(template).spec.runner || {};
  const runnerChanged = [
    ["#runner-platform", thresholdRunner.platforms?.[0] || ""],
    ["#runner-memory", thresholdRunner.memory || ""],
    ["#runner-compute", thresholdRunner.compute || ""],
    ["#runner-storage", thresholdRunner.storage || ""],
  ].some(([selector, value]) => document.querySelector(selector).value !== value);
  const delta = document.querySelector("#request-delta");
  delta.replaceChildren();
  if (budget === null || ceiling === null) text(delta, "Enter a valid budget to preview the bounded request.");
  else if (budget > ceiling) text(delta, `Outside the hard ceiling of ${template.ceiling.spec.budget.monthlyLimit} ${template.ceiling.spec.budget.currency}; Steward will reject this request.`);
  else if (threshold === null) text(delta, "No automatic provisioning authority is recorded. This bounded request will be routed for review.");
  else if (budget > threshold || ttl !== template.autoProvisionThreshold.spec.ttl || runnerChanged) text(delta, "Within the hard ceiling but outside the automatic threshold; server-side validation will determine whether this request is pending approval.");
  else text(delta, "Within the automatic threshold. Steward will attempt provisioning after server-side validation.");
}

function detailValue(id, value) {
  text(document.querySelector(`#${id}`), value);
}

function listAuthority(values, render, unavailable) {
  return values?.length ? values.map(render).join(", ") : unavailable;
}

function runnerValue(runner, field, unavailable) {
  if (!runner) return unavailable;
  if (field === "platform") return runner.platforms?.length ? runner.platforms.join(", ") : unavailable;
  return runner[field] || unavailable;
}

function renderEnvelopeAuthority(request) {
  const requested = request.requestedEnvelope?.spec;
  const approved = request.approvedEnvelope?.spec;
  const unknownApproval = "Not recorded by the current approval authority.";
  const unknownRunner = "Not recorded by the current envelope authority.";
  detailValue("requested-tools", listAuthority(requested?.tools, (tool) => `${tool.provider}:${tool.resource}:${tool.action}`, "None requested."));
  detailValue("approved-tools", listAuthority(approved?.tools, (tool) => `${tool.provider}:${tool.resource}:${tool.action}`, unknownApproval));
  detailValue("requested-models", listAuthority(requested?.llms, (model) => `${model.provider}/${model.model}`, "None requested."));
  detailValue("approved-models", listAuthority(approved?.llms, (model) => `${model.provider}/${model.model}`, unknownApproval));
  detailValue("requested-budget", requested?.budget ? `${requested.budget.monthlyLimit} ${requested.budget.currency}` : "Not recorded.");
  detailValue("approved-budget", approved?.budget ? `${approved.budget.monthlyLimit} ${approved.budget.currency}` : unknownApproval);
  detailValue("requested-runtime", requested?.ttl ?? "Not recorded.");
  detailValue("approved-runtime", approved?.ttl ?? unknownApproval);
  for (const field of ["platform", "memory", "compute", "storage"]) {
    detailValue(`requested-${field}`, runnerValue(requested?.runner, field, unknownRunner));
    detailValue(`approved-${field}`, runnerValue(approved?.runner, field, unknownApproval));
  }
}

function workflowPanelFor(request) {
  const panel = document.querySelector("#github-actions-workflow");
  const message = document.querySelector("#github-actions-workflow-message");
  const canRender = request.status === "provisioned"
    && request.approvedEnvelope
    && request.envelopeInstanceId
    && request.envelopeDigest;
  panel.hidden = !canRender;
  if (!canRender) text(message, "");
  return canRender ? panel : null;
}

async function generateGithubActionsWorkflow(event) {
  event.preventDefault();
  const form = document.querySelector("#github-actions-workflow-form");
  const message = document.querySelector("#github-actions-workflow-message");
  const button = document.querySelector("#generate-github-actions-workflow");
  const requestId = form.dataset.requestId;
  button.disabled = true;
  text(message, "Generating exact YAML…");
  try {
    const session = await csrf();
    const response = await fetch(`${API}/envelope-requests/${requestId}/github-actions-workflow`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-steward-csrf": session.csrf },
      body: JSON.stringify({
        repository: document.querySelector("#github-repository").value.trim(),
        revision: document.querySelector("#github-revision").value.trim(),
        path: document.querySelector("#github-path").value.trim(),
      }),
    });
    if (!response.ok) throw new Error(`Workflow could not be generated (${response.status}).`);
    const payload = await response.json();
    const workflow = payload.workflow;
    if (workflow?.contentType !== "application/yaml" || typeof workflow.yaml !== "string" || typeof workflow.sha256 !== "string") {
      throw new Error("The generated workflow did not match the reviewed contract.");
    }
    document.querySelector("#generated-github-actions-yaml").value = workflow.yaml;
    text(document.querySelector("#generated-github-actions-digest"), workflow.sha256);
    document.querySelector("#generated-github-actions-workflow").hidden = false;
    text(message, "Exact workflow YAML is ready to copy.");
  } catch (error) { text(message, error.message); }
  finally { button.disabled = false; }
}

async function copyGithubActionsWorkflow() {
  const yaml = document.querySelector("#generated-github-actions-yaml");
  const message = document.querySelector("#github-actions-workflow-message");
  try {
    await navigator.clipboard.writeText(yaml.value);
    text(message, "Copied exact workflow YAML to the clipboard.");
  } catch (_) {
    yaml.focus();
    yaml.select();
    text(message, "Clipboard access was denied. Select and copy the YAML manually.");
  }
}

async function csrf() { return (await fetch("/admin/api/v1/session")).json(); }

async function loadNewEnvelope() {
  const message = document.querySelector("#envelope-form-message");
  const form = document.querySelector("#envelope-request-form");
  const select = document.querySelector("#template-select");
  try {
    const response = await api("/envelope-templates");
    const templates = response.templates;
    select.replaceChildren(...templates.map((template) => { const option = create("option", `${template.displayName} · revision ${template.revision}`); option.value = template.id; return option; }));
    const selected = new URLSearchParams(window.location.search).get("template");
    if (selected && templates.some((template) => template.id === selected)) select.value = selected;
    if (!templates.length) { text(message, "No entries."); return; }
    form.hidden = false;
    text(message, "");
    const update = () => renderTemplate(selectedTemplate(templates, select));
    select.addEventListener("change", update);
    document.querySelector("#budget-limit").addEventListener("input", () => renderDelta(selectedTemplate(templates, select)));
    for (const selector of ["#model-select", "#tool-select", "#connection-select", "#ttl", "#runner-platform", "#runner-memory", "#runner-compute", "#runner-storage"]) {
      document.querySelector(selector).addEventListener("input", () => renderDelta(selectedTemplate(templates, select)));
      document.querySelector(selector).addEventListener("change", () => renderDelta(selectedTemplate(templates, select)));
    }
    update();
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const template = selectedTemplate(templates, select);
      const requestedEnvelope = structuredClone(templateBaseline(template));
      requestedEnvelope.revision = template.revision;
      const selectedModels = new Set(selectedChoiceValues(document.querySelector("#model-select")));
      const selectedTools = new Set(selectedChoiceValues(document.querySelector("#tool-select")));
      requestedEnvelope.spec.llms = template.ceiling.spec.llms.filter((model) => selectedModels.has(modelLabel(model)));
      requestedEnvelope.spec.tools = template.ceiling.spec.tools.filter((tool) => selectedTools.has(toolLabel(tool)));
      requestedEnvelope.spec.budget.monthlyLimit = document.querySelector("#budget-limit").value;
      requestedEnvelope.spec.ttl = document.querySelector("#ttl").value;
      requestedEnvelope.spec.runner = {
        platforms: document.querySelector("#runner-platform").value ? [document.querySelector("#runner-platform").value] : [],
        memory: document.querySelector("#runner-memory").value || null,
        compute: document.querySelector("#runner-compute").value || null,
        storage: document.querySelector("#runner-storage").value || null,
      };
      const session = await csrf();
      const response = await fetch(`${API}/envelope-requests`, { method: "POST", headers: { "content-type": "application/json", "x-steward-csrf": session.csrf }, body: JSON.stringify({ templateId: template.id, templateRevision: template.revision, requestedEnvelope, idempotencyKey: crypto.randomUUID() }) });
      if (!response.ok) { text(message, `Request was not accepted (${response.status}). Review the bounded request and connection readiness.`); return; }
      const created = await response.json();
      window.location.assign(`/envelopes/${created.request.id}`);
    });
  } catch (error) { text(message, error.message); }
}

async function loadDetail() {
  const message = document.querySelector("#envelope-detail-message");
  const container = document.querySelector("#envelope-detail");
  const details = container.querySelector("dl");
  try {
    const response = await api(`/envelope-requests/${requestId()}`);
    const request = response.request;
    const rows = [["Status", request.status], ["Template", `${request.templateId} · revision ${request.templateRevision}`], ["Requested", request.createdAt], ["Status recorded", request.statusAt], ["Envelope instance", request.envelopeInstanceId || "Not provisioned"], ["Digest", request.envelopeDigest || "Not provisioned"], ["Reason", request.reason || "None recorded"]];
    details.replaceChildren(...rows.flatMap(([term, value]) => [create("dt", term), create("dd", value)]));
    renderEnvelopeAuthority(request);
    const workflowPanel = workflowPanelFor(request);
    if (workflowPanel) workflowPanel.querySelector("#github-actions-workflow-form").dataset.requestId = request.id;
    container.querySelector("#envelope-runs-link").href = `/envelopes/${request.id}/runs`;
    container.hidden = false;
    text(message, "");
  } catch (error) { text(message, error.message); }
}

function renderRun(run) {
  const card = create("article");
  card.className = "card";
  const heading = create("h2");
  const link = create("a", run.workflow);
  link.href = `/runs/${run.taskUid}`;
  heading.append(link);
  card.append(heading, create("p", `Phase: ${run.phase}. Updated ${run.updatedAt}.`));
  return card;
}

async function loadEnvelopeRuns() {
  const message = document.querySelector("#envelope-runs-message");
  const list = document.querySelector("#envelope-runs-list");
  try {
    const requestResponse = await api(`/envelope-requests/${envelopeIdForRuns()}`);
    const request = requestResponse.request;
    const runsResponse = request.envelopeInstanceId
      ? await runsApi("", { runtimeUid: request.envelopeInstanceId })
      : { runs: [] };
    const runs = runsResponse.runs;
    list.replaceChildren(...(runs.length ? runs.map(renderRun) : [create("p", "No entries.")]));
    text(message, "");
  } catch (error) { text(message, error.message); }
}

async function loadRuns() {
  const message = document.querySelector("#runs-message");
  const list = document.querySelector("#runs-list");
  try {
    const response = await runsApi();
    list.replaceChildren(...(response.runs.length ? response.runs.map(renderRun) : [create("p", "No entries.")]));
    text(message, "");
  } catch (error) { text(message, error.message); }
}

async function loadAdminWorkspace() {
  const message = document.querySelector("#admin-runs-message");
  const list = document.querySelector("#admin-runs-list");
  try {
    const response = await runsApi();
    list.replaceChildren(...(response.runs.length ? response.runs.map(renderRun) : [create("p", "No entries.")]));
    text(message, "");
  } catch (error) { text(message, error.message); }
}

function timelineLabel(event) {
  if (event.kind === "phase") return `Phase changed to ${event.phase}.`;
  if (event.kind === "finalizationRequested") return "Finalization requested.";
  return "Finalized.";
}

async function loadRunDetail() {
  const message = document.querySelector("#run-detail-message");
  const container = document.querySelector("#run-detail");
  const details = container.querySelector("dl");
  const timeline = container.querySelector("#run-timeline");
  try {
    const [detailResponse, timelineResponse] = await Promise.all([
      runsApi(`/${runId()}`), runsApi(`/${runId()}/timeline`),
    ]);
    const run = detailResponse.run;
    const rows = [
      ["Workflow", run.workflow], ["Phase", run.phase], ["Agent runtime", run.codingAgentRuntime],
      ["Runtime", run.runtimeUid || "Not bound"], ["Created", run.createdAt], ["Updated", run.updatedAt],
      ["Envelope revision", run.envelopeRevision ?? "Not recorded"], ["Observed spend", run.observedSpend ? `${run.observedSpend.observedAmount} ${run.observedSpend.currency}` : "Not observed"],
      ["Failure category", run.errorCategory || "None recorded"],
    ];
    details.replaceChildren(...rows.flatMap(([term, value]) => [create("dt", term), create("dd", String(value))]));
    timeline.replaceChildren(...(timelineResponse.events.length ? timelineResponse.events.map((event) => {
      const item = create("li", timelineLabel(event));
      const when = create("time", event.at);
      when.dateTime = event.at;
      item.append(when);
      return item;
    }) : [create("li", "No entries.")]));
    container.hidden = false;
    text(message, "");
  } catch (error) { text(message, error.message); }
}

setupAccordions();
document.querySelector("#github-actions-workflow-form")?.addEventListener("submit", generateGithubActionsWorkflow);
document.querySelector("#copy-github-actions-workflow")?.addEventListener("click", copyGithubActionsWorkflow);
loadIdentity();
if (activePage() === "/envelopes") loadEnvelopeGroups();
if (activePage() === "/envelopes/new") loadNewEnvelope();
if (activePage() === "/envelopes/detail") loadDetail();
if (activePage() === "/envelopes/runs") loadEnvelopeRuns();
if (activePage() === "/runs") loadRuns();
if (activePage() === "/runs/detail") loadRunDetail();
if (activePage() === "/admin/workspace") loadAdminWorkspace();
