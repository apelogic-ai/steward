"use strict";

const API = "/app/api/v1";
const path = window.location.pathname;
const pages = Array.from(document.querySelectorAll("[data-page]"));
const links = Array.from(document.querySelectorAll("[data-route]"));

function activePage() {
  if (/^\/app\/envelopes\/[0-9a-f-]{36}$/i.test(path)) return "/app/envelopes/detail";
  return path;
}

for (const page of pages) page.hidden = page.dataset.page !== activePage();
for (const link of links) if (link.dataset.route === path) link.setAttribute("aria-current", "page");

function text(element, value) { element.textContent = value; }
function create(tag, value) { const element = document.createElement(tag); if (value) text(element, value); return element; }

async function api(pathname, options) {
  const response = await fetch(`${API}${pathname}`, options);
  if (!response.ok) throw new Error(`The authoritative request failed (${response.status}).`);
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
  link.href = `/app/envelopes/${request.id}`;
  heading.append(link);
  card.append(heading, renderStatus(request));
  card.append(create("p", `Requested ${request.createdAt}. Current status recorded ${request.statusAt}.`));
  return card;
}

async function loadList() {
  const message = document.querySelector("#envelope-list-message");
  const list = document.querySelector("#envelope-list");
  try {
    const response = await api("/envelope-requests");
    list.replaceChildren(...response.requests.map(renderRequestSummary));
    text(message, response.requests.length ? "" : "No envelope requests yet.");
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
  if (budget === null || ceiling === null || threshold === null) {
    text(delta, "Enter a valid budget to preview the bounded request.");
  } else if (budget > ceiling) {
    text(delta, `Outside the hard ceiling of ${template.ceiling.spec.budget.monthlyLimit} ${template.ceiling.spec.budget.currency}; Steward will reject this request.`);
  } else if (budget > threshold || ttl !== template.autoProvisionThreshold.spec.ttl) {
    text(delta, "Within the hard ceiling but outside the automatic threshold; this request will be pending approval.");
  } else {
    text(delta, "Within the automatic threshold. Steward will attempt provisioning after server-side validation.");
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
    select.replaceChildren(...templates.map((template) => {
      const option = create("option", `${template.displayName} · revision ${template.revision}`);
      option.value = template.id;
      return option;
    }));
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
      const response = await fetch(`${API}/envelope-requests`, {
        method: "POST",
        headers: { "content-type": "application/json", "x-steward-csrf": session.csrf },
        body: JSON.stringify({ templateId: template.id, templateRevision: template.revision, requestedEnvelope, idempotencyKey: crypto.randomUUID() }),
      });
      if (!response.ok) { text(message, `Request was not accepted (${response.status}). Review the bounded request and connection readiness.`); return; }
      const created = await response.json();
      window.location.assign(`/app/envelopes/${created.request.id}`);
    });
  } catch (error) { text(message, error.message); }
}

async function loadDetail() {
  const id = path.split("/").at(-1);
  const message = document.querySelector("#envelope-detail-message");
  const details = document.querySelector("#envelope-detail");
  try {
    const response = await api(`/envelope-requests/${id}`);
    const request = response.request;
    const rows = [
      ["Status", request.status], ["Template", `${request.templateId} · revision ${request.templateRevision}`],
      ["Requested", request.createdAt], ["Status recorded", request.statusAt],
      ["Envelope instance", request.envelopeInstanceId || "Not provisioned"],
      ["Digest", request.envelopeDigest || "Not provisioned"], ["Reason", request.reason || "None recorded"],
    ];
    details.replaceChildren(...rows.flatMap(([term, value]) => [create("dt", term), create("dd", value)]));
    details.hidden = false;
    text(message, "");
  } catch (error) { text(message, error.message); }
}

if (path === "/app/envelopes") loadList();
if (path === "/app/envelopes/new") loadNewEnvelope();
if (/^\/app\/envelopes\/[0-9a-f-]{36}$/i.test(path)) loadDetail();
