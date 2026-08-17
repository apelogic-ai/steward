"use strict";

const API = "/app/api/v1";
const ACCORDION_PREFIX = "steward.ui.envelope-accordion.";
const ACCORDIONS = new Set(["templates", "drafts", "approved", "in-review"]);
const path = window.location.pathname;
const pages = Array.from(document.querySelectorAll("[data-page]"));

function activePage() {
  if (/^\/runs\/[0-9a-f-]{36}$/i.test(path)) return "/runs/detail";
  if (/^\/envelopes\/[^/]+\/runs$/i.test(path)) return "/envelopes/runs";
  if (/^\/envelopes\/[^/]+$/i.test(path)) return path.endsWith("/new") ? "/envelopes/new" : "/envelopes/detail";
  if (path === "/app/envelopes") return "/envelopes";
  if (path === "/app/envelopes/new") return "/envelopes/new";
  if (/^\/app\/envelopes\/[0-9a-f-]{36}$/i.test(path)) return "/envelopes/detail";
  return path;
}

for (const page of pages) page.hidden = page.dataset.page !== activePage();
for (const link of document.querySelectorAll("[data-route]")) {
  if (link.dataset.route === activePage()) link.setAttribute("aria-current", "page");
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
    try { details.open = localStorage.getItem(`${ACCORDION_PREFIX}${name}`) === "open"; } catch (_) { /* Preferences are optional. */ }
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
  } catch (_) { text(target, "Session unavailable"); }
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
  const endpoint = session.role === "admin" ? "/admin/api/v1/all-runs" : `${API}/runs`;
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
    templates.replaceChildren(...(templateResponse.templates.length ? templateResponse.templates.map(renderTemplateSummary) : [create("p", "No templates are authorized for your identity.")]));
    text(drafts, "No saved drafts are exposed by the authoritative envelope API.");
    const records = requestResponse.requests;
    renderGroup(approved, records.filter((record) => ["approved", "provisioned"].includes(record.status)), "No approved envelopes.");
    renderGroup(review, records.filter((record) => !["approved", "provisioned"].includes(record.status)), "No envelopes require review.");
    text(message, "");
  } catch (error) { text(message, error.message); }
}

function selectedTemplate(templates, select) { return templates.find((template) => template.id === select.value); }
function decimal(value) { const parsed = Number(value); return Number.isFinite(parsed) ? parsed : null; }

function renderTemplate(template) {
  const summary = document.querySelector("#template-summary");
  summary.replaceChildren();
  summary.append(create("strong", `${template.displayName} · revision ${template.revision}`));
  summary.append(create("p", `Models: ${template.ceiling.spec.llms.map((model) => `${model.provider}/${model.model}`).join(", ")}.`));
  summary.append(create("p", `Tools: ${template.ceiling.spec.tools.map((tool) => `${tool.provider}:${tool.resource}:${tool.action}`).join(", ")}.`));
  summary.append(create("p", `GitHub connection: ${template.githubConnection}.`));
  document.querySelector("#budget-limit").value = template.autoProvisionThreshold.spec.budget.monthlyLimit;
  document.querySelector("#ttl").value = template.autoProvisionThreshold.spec.ttl;
  renderDelta(template);
}

function renderDelta(template) {
  const budget = decimal(document.querySelector("#budget-limit").value);
  const ceiling = decimal(template.ceiling.spec.budget.monthlyLimit);
  const threshold = decimal(template.autoProvisionThreshold.spec.budget.monthlyLimit);
  const ttl = document.querySelector("#ttl").value;
  const delta = document.querySelector("#request-delta");
  delta.replaceChildren();
  if (budget === null || ceiling === null || threshold === null) text(delta, "Enter a valid budget to preview the bounded request.");
  else if (budget > ceiling) text(delta, `Outside the hard ceiling of ${template.ceiling.spec.budget.monthlyLimit} ${template.ceiling.spec.budget.currency}; Steward will reject this request.`);
  else if (budget > threshold || ttl !== template.autoProvisionThreshold.spec.ttl) text(delta, "Within the hard ceiling but outside the automatic threshold; this request will be pending approval.");
  else text(delta, "Within the automatic threshold. Steward will attempt provisioning after server-side validation.");
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
    if (!templates.length) { text(message, "No envelope templates are available to your identity."); return; }
    form.hidden = false;
    text(message, "");
    const update = () => renderTemplate(selectedTemplate(templates, select));
    select.addEventListener("change", update);
    document.querySelector("#budget-limit").addEventListener("input", () => renderDelta(selectedTemplate(templates, select)));
    document.querySelector("#ttl").addEventListener("input", () => renderDelta(selectedTemplate(templates, select)));
    update();
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const template = selectedTemplate(templates, select);
      const requestedEnvelope = structuredClone(template.ceiling);
      requestedEnvelope.revision = template.revision;
      requestedEnvelope.spec.budget.monthlyLimit = document.querySelector("#budget-limit").value;
      requestedEnvelope.spec.ttl = document.querySelector("#ttl").value;
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
    list.replaceChildren(...(runs.length ? runs.map(renderRun) : [create("p", request.envelopeInstanceId ? "No recent runs are recorded for this envelope instance." : "This envelope is not provisioned, so it has no runs.")]));
    text(message, "");
  } catch (error) { text(message, error.message); }
}

async function loadRuns() {
  const message = document.querySelector("#runs-message");
  const list = document.querySelector("#runs-list");
  try {
    const response = await runsApi();
    list.replaceChildren(...(response.runs.length ? response.runs.map(renderRun) : [create("p", "No runs are recorded for your identity.")]));
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
    }) : [create("li", "No lifecycle events are recorded.")]));
    container.hidden = false;
    text(message, "");
  } catch (error) { text(message, error.message); }
}

setupAccordions();
loadIdentity();
if (activePage() === "/envelopes") loadEnvelopeGroups();
if (activePage() === "/envelopes/new") loadNewEnvelope();
if (activePage() === "/envelopes/detail") loadDetail();
if (activePage() === "/envelopes/runs") loadEnvelopeRuns();
if (activePage() === "/runs") loadRuns();
if (activePage() === "/runs/detail") loadRunDetail();
