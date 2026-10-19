import { expect } from "@playwright/test";

import { isIssue414RunFailure } from "../run-failure-markers.mjs";
import { defaultWorkspaceName } from "../workspace-name.mjs";
import { Given, Then, When } from "./fixtures.mjs";

const runPrefix = process.env.STEWARD_JOURNEY_RUN_PREFIX;
const memberRole = process.env.STEWARD_JOURNEY_MEMBER_ROLE;
const repository = process.env.STEWARD_JOURNEY_REPOSITORY;
const configuredAgent = process.env.STEWARD_JOURNEY_AGENT;
const configuredModel = process.env.STEWARD_JOURNEY_MODEL;
const envelopeDetailPath = /\/envelopes\/(?!new$)[A-Za-z0-9-]+$/;

function runtimeMinuteSeed() {
  let hash = 2166136261;
  for (const character of runPrefix) {
    hash ^= character.codePointAt(0);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0) % 10_000_000;
}

function formatRuntimeMinutes(offset) {
  const microMinutes = 50_000_000 + offset;
  return `${Math.floor(microMinutes / 1_000_000)}.${String(microMinutes % 1_000_000).padStart(6, "0")}`;
}

function templateId(name) {
  return `${runPrefix}-${name}`;
}

function resolveText(journey, text) {
  const workspaceName = defaultWorkspaceName(repository.split("/").at(-1));
  let prefixed = text
    .replaceAll("<run-prefix>", runPrefix)
    .replaceAll("<owner>/<repository>", repository)
    .replaceAll("<workspace-name>", workspaceName);
  if (prefixed === "member") return memberRole;
  if (["Agent choice", "Read Jira", "Read the repository", "Scratch and Git", "Scratch check", "Stated needs", "Zero per-run budget"].includes(prefixed)) {
    return `${runPrefix}-${prefixed.toLocaleLowerCase("en-US").replaceAll(/[^a-z0-9]+/g, "-").replaceAll(/^-|-$/g, "")}`;
  }
  if (prefixed === "<run-runtime-minutes>") {
    if (!journey.runRuntimeMinutes) throw new Error("the journey must choose an unused runtime-minutes value before filling the request");
    return journey.runRuntimeMinutes;
  }
  for (const [alias, value] of journey.modelAliases) prefixed = prefixed.replaceAll(alias, value);
  return prefixed;
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function fieldLocator(page, field) {
  const name = new RegExp(`^${escapeRegExp(field)}(?:\\s|$)`, "i");
  if (["Coding agent", "Eligible member roles", "Envelope", "Model", "Models", "Package source", "Template", "Workspace repository"].includes(field)) {
    return page.getByRole("combobox", { name }).first();
  }
  if (["Limit per run", "Monthly limit", "Time-to-live limit", "Runtime limit per month"].includes(field)) {
    return page.getByRole("spinbutton", { name }).first();
  }
  if (["Branch or tag", "Display name", "Expires at (optional)", "Name", "New template ID", "Package path", "Prompt", "Rationale", "Requirements (optional JSON)", "Template ID", "Total size limit (optional)", "Workspace (advanced JSON array)"].includes(field)) {
    return page.getByRole("textbox", { name }).first();
  }
  if (field === "05 Request note") {
    return page.getByRole("textbox", { name: /^Justification\s/i }).first();
  }
  return page.getByLabel(name).first();
}

function checkboxLocator(page, field) {
  return page.getByRole("checkbox", { name: new RegExp(`^${escapeRegExp(field)}(?:\\s|$)`, "i") });
}

function templateTable(page) {
  return page.getByRole("table", { name: "Envelope templates" });
}

function prefixedTemplateRows(page) {
  return templateTable(page).getByRole("rowgroup").getByRole("link").filter({ hasText: runPrefix });
}

async function waitForTemplateList(page) {
  await expect.poll(async () => {
    const tableCount = await templateTable(page).count();
    const emptyCount = await page.getByText("No templates yet.", { exact: true }).count();
    return tableCount + emptyCount;
  }, { message: "the Templates page should finish loading its table or empty state" }).toBeGreaterThan(0);
}

async function waitForCollection(page, tableName, emptyText) {
  await expect.poll(async () => {
    const tableCount = await page.getByRole("table", { name: tableName }).count();
    const emptyCount = await page.getByText(emptyText, { exact: true }).count();
    return tableCount + emptyCount;
  }, { message: `${tableName} should finish loading its table or empty state` }).toBeGreaterThan(0);
}

async function assertSignedIn(page) {
  try {
    await expect(page.getByRole("navigation", { name: "Primary navigation" })).toBeVisible({ timeout: 10_000 });
  } catch (error) {
    const signInPath = new URL(page.url()).pathname === "/admin/sign-in";
    const signInHeading = await page.getByRole("heading", { name: /sign in|signed out/i }).count();
    if (signInPath || signInHeading > 0) {
      throw new Error("The signed-in Playwright storage state is missing or expired. Sign in manually, save a fresh storage-state file, and retry.");
    }
    throw error;
  }
}

function areaLocator(page, area) {
  switch (area) {
    case "Admin workspace":
      return page.locator("aside").filter({ hasText: "Admin workspace" });
    case "Templates":
      return page.getByRole("table", { name: "Envelope templates" });
    case "Needs action":
      return page.getByRole("table", { name: "Requests" });
    case "Envelopes":
      return page.getByRole("table", { name: "Envelopes" });
    case "Runs":
      return page.getByRole("table", { name: "Runs" });
    case "Limit per run":
    case "Monthly limit":
    case "Runtime limit per month":
      return fieldLocator(page, area).locator("xpath=ancestor::label[1]");
    case "Access":
      return page.locator("dt").filter({ hasText: /^Access$/ }).first().locator("xpath=following-sibling::dd[1]");
    case "Jira":
      return page.locator('[data-provider-action-footer="jira"]').locator("xpath=ancestor::section[1]");
    case "Tools":
    case "Workspace access":
      return page.getByRole("heading", { name: area, exact: true }).first().locator("xpath=ancestor::section[1]");
    case "Repository details":
      return page.locator("details").filter({ has: page.locator("summary", { hasText: area }) }).first();
    default:
      return page.getByRole("heading", { name: area, exact: true }).first().locator("xpath=ancestor::section[1]");
  }
}

async function apiJson(page, path) {
  if (page.url() === "about:blank") await page.goto("/");
  const response = await page.request.get(new URL(path, page.url()).toString());
  expect(response.ok(), `${path} should be available on the installation`).toBeTruthy();
  return response.json();
}

async function mutationJson(page, path, method, body) {
  const session = await apiJson(page, "/admin/api/v1/session");
  const result = await page.evaluate(async ({ csrf, mutationBody, mutationMethod, mutationPath }) => {
    const response = await fetch(mutationPath, {
      body: JSON.stringify(mutationBody),
      cache: "no-store",
      credentials: "same-origin",
      headers: { "content-type": "application/json", "X-Steward-CSRF": csrf },
      method: mutationMethod,
    });
    return { body: await response.text(), ok: response.ok, status: response.status };
  }, { csrf: session.csrf, mutationBody: body, mutationMethod: method, mutationPath: path });
  let parsed = null;
  try {
    parsed = result.body ? JSON.parse(result.body) : null;
  } catch {
    // Keep the response status as the public failure signal; never echo an HTML error body.
  }
  expect(result.ok, `${method} ${path} should succeed (status ${result.status})`).toBeTruthy();
  return parsed;
}

async function userTemplates(page) {
  const data = await apiJson(page, "/app/api/v1/envelope-templates");
  expect(Array.isArray(data.templates), "the Envelope template collection should be valid").toBeTruthy();
  return data.templates;
}

async function ensureExtendedTemplate(page, name) {
  let templates = await userTemplates(page);
  let template = templates.find((candidate) => candidate.id === templateId(name));
  expect(["basic", "broad", "jira-read"], `the journey fixture does not author the ${name} template`).toContain(name);
  if (template && name !== "jira-read") return template;
  const standard = templates.find((candidate) => candidate.id === templateId("standard"));
  expect(standard, `the ${templateId("standard")} template must be created by @smoke before ${name === "jira-read" ? "@needs-jira" : "@extended"}`).toBeDefined();
  if (name === "jira-read") {
    const [catalog, connections] = await Promise.all([
      apiJson(page, "/admin/api/v1/capabilities"),
      apiJson(page, "/app/api/v1/connections"),
    ]);
    const available = connections.available.find((candidate) => candidate.provider === "jira");
    expect(available?.enabled, "the installation should enable Jira connections").toBeTruthy();
    const connection = connections.connections.find((candidate) => candidate.provider === "jira");
    expect(connection?.status?.phase, "Jira should be connected for the signed-in account before @needs-jira runs").toBe("connected");
    const tool = catalog.tools.find((candidate) => (
      candidate.provider === "jira"
      && candidate.resource === "jira_search_issues"
      && candidate.action === "read"
      && candidate.accessClass === "read"
    ));
    expect(tool, "the deployment catalog should contain the Jira read-only search tool").toBeDefined();
    if (template) {
      expect(template.ceiling.spec.tools).toEqual([{ provider: tool.provider, resource: tool.resource, action: tool.action }]);
      return template;
    }
    const envelope = structuredClone(standard.ceiling);
    envelope.revision = 1;
    envelope.spec.tools = [{ provider: tool.provider, resource: tool.resource, action: tool.action }];
    delete envelope.spec.workspace;
    await mutationJson(page, `/admin/api/v1/envelope-templates/${encodeURIComponent(templateId(name))}`, "PUT", {
      allowInlineBrowserTasks: true,
      displayName: `${runPrefix} Jira read`,
      envelope,
      memberRoles: [memberRole],
    });
    templates = await userTemplates(page);
    template = templates.find((candidate) => candidate.id === templateId(name));
    expect(template, `the ${templateId(name)} template should be available after authoring`).toBeDefined();
    return template;
  }
  const [capabilities, catalog] = await Promise.all([
    apiJson(page, "/app/api/v1/capabilities"),
    apiJson(page, "/admin/api/v1/capabilities"),
  ]);
  const compatibleModels = [];
  for (const agent of capabilities.agents) {
    const model = catalog.models.find((candidate) => agent.models.providers.includes(candidate.provider));
    if (model && !compatibleModels.some((candidate) => candidate.provider === model.provider && candidate.model === model.model)) {
      compatibleModels.push(model);
    }
  }
  const needed = name === "broad" ? 2 : 1;
  expect(compatibleModels.length, `${name} requires ${needed} configured agent-compatible model(s)`).toBeGreaterThanOrEqual(needed);
  const envelope = structuredClone(standard.ceiling);
  envelope.revision = 1;
  envelope.spec.llms = compatibleModels.slice(0, needed);
  delete envelope.spec.workspace;
  await mutationJson(page, `/admin/api/v1/envelope-templates/${encodeURIComponent(templateId(name))}`, "PUT", {
    allowInlineBrowserTasks: true,
    displayName: `${runPrefix} ${name.charAt(0).toUpperCase()}${name.slice(1)}`,
    envelope,
    memberRoles: [memberRole],
  });
  templates = await userTemplates(page);
  template = templates.find((candidate) => candidate.id === templateId(name));
  expect(template, `the ${templateId(name)} template should be available after authoring`).toBeDefined();
  return template;
}

async function userRequests(page, status = null) {
  const requests = [];
  const seenCursors = new Set();
  let cursor = null;
  do {
    const query = new URLSearchParams({ limit: "100" });
    if (status) query.set("status", status);
    if (cursor) query.set("cursor", cursor);
    const data = await apiJson(page, `/app/api/v1/envelope-requests?${query}`);
    expect(Array.isArray(data.requests), "the Envelope request collection should be valid").toBeTruthy();
    requests.push(...data.requests);
    cursor = data.nextCursor ?? null;
    if (cursor) {
      expect(seenCursors.has(cursor), "the Envelope request cursor must advance").toBeFalsy();
      seenCursors.add(cursor);
    }
  } while (cursor);
  return requests;
}

async function userRuns(page) {
  const runs = [];
  const seenCursors = new Set();
  let cursor = null;
  do {
    const query = new URLSearchParams({ limit: "100" });
    if (cursor) query.set("cursor", cursor);
    const data = await apiJson(page, `/app/api/v1/runs?${query}`);
    expect(Array.isArray(data.runs), "the Run collection should be valid").toBeTruthy();
    runs.push(...data.runs);
    cursor = data.nextCursor ?? null;
    if (cursor) {
      expect(seenCursors.has(cursor), "the Run cursor must advance").toBeFalsy();
      seenCursors.add(cursor);
    }
  } while (cursor);
  return runs;
}

async function recordInitialRuns(journey, page) {
  if (journey.initialRunIds) return;
  journey.initialRunIds = new Set((await userRuns(page)).map((run) => run.taskUid));
}

async function provisionEnvelope(journey, page, template) {
  const requestedEnvelope = structuredClone(template.ceiling);
  requestedEnvelope.spec.runtimeMinutesLimit = journey.runRuntimeMinutes;
  const response = await mutationJson(page, "/app/api/v1/envelope-requests", "POST", {
    idempotencyKey: crypto.randomUUID(),
    requestedEnvelope,
    templateId: template.id,
    templateRevision: template.revision,
  });
  expect(response?.request?.status, `${template.id} should auto-provision within its template limits`).toBe("provisioned");
  return response.request;
}

async function provisionExtendedEnvelope(journey, page, name) {
  return provisionEnvelope(journey, page, await ensureExtendedTemplate(page, name));
}

async function chooseUnusedRuntimeMinutes(page) {
  const active = await userRequests(page, "provisioned");
  const used = new Set(active.flatMap((request) => {
    const value = (request.approvedEnvelope ?? request.requestedEnvelope)?.spec?.runtimeMinutesLimit;
    return value ? [value] : [];
  }));
  const seed = runtimeMinuteSeed();
  for (let attempt = 0; attempt < 10_000_000; attempt += 1) {
    const candidate = formatRuntimeMinutes((seed + attempt) % 10_000_000);
    if (!used.has(candidate)) return candidate;
  }
  throw new Error("No unused six-decimal runtime-minutes value remains between 50 and 60 min.");
}

async function requestForTemplate(page, name, status) {
  const matches = (await userRequests(page))
    .filter((request) => request.templateId === templateId(name) && (!status || request.status === status))
    .sort((left, right) => Date.parse(right.createdAt) - Date.parse(left.createdAt));
  return matches[0] ?? null;
}

async function chooseFirstOfferedModel(page) {
  const picker = page.getByRole("combobox", { name: "Model", exact: true });
  if (!(await picker.count())) return;
  await expect.poll(() => picker.locator("option:not([disabled])").count(), {
    message: "the selected Envelope should offer a compatible model",
  }).toBeGreaterThan(0);
  const value = await picker.locator("option:not([disabled])").first().getAttribute("value");
  expect(value, "the first offered model should have a value").toBeTruthy();
  await picker.selectOption(value);
}

async function selectConfiguredOption(picker, configured, field) {
  let match;
  await expect.poll(async () => {
    const options = await picker.locator("option").evaluateAll((elements) => elements.map((element) => ({
      disabled: element.disabled,
      label: element.textContent?.trim() ?? "",
      value: element.value,
    })));
    match = options.find((option) => !option.disabled && (
      option.value === configured
        || option.label === configured
        || option.label.startsWith(`${configured} ·`)
    ));
    return Boolean(match);
  }, { message: `${field} should offer the configured value ${configured}` }).toBeTruthy();
  await picker.selectOption(match.value);
}

async function selectConfiguredRuntime(page, envelopeSpec, envelopeName) {
  const exactTaskNotice = page.getByText(
    "This exact package cannot be changed here. Choose an Envelope and optional inputs for this run.",
    { exact: true },
  );
  if (await exactTaskNotice.count()) return;

  const allowedModels = envelopeSpec?.llms ?? [];
  let configuredBinding = null;
  let configuredModelEntry = null;
  if (configuredAgent || configuredModel) {
    const [capabilities, catalog] = await Promise.all([
      apiJson(page, "/app/api/v1/capabilities"),
      apiJson(page, "/admin/api/v1/capabilities"),
    ]);
    if (configuredAgent) {
      configuredBinding = capabilities.agents.find((candidate) => (
        candidate.agentRef === configuredAgent || candidate.displayName === configuredAgent
      ));
      expect(configuredBinding, `the installation should configure ${configuredAgent}`).toBeDefined();
    }
    if (configuredModel) {
      const separator = configuredModel.indexOf("/");
      expect(separator, "STEWARD_JOURNEY_MODEL should use provider/model form").toBeGreaterThan(0);
      const provider = configuredModel.slice(0, separator);
      const model = configuredModel.slice(separator + 1);
      expect(model, "STEWARD_JOURNEY_MODEL should use provider/model form").not.toBe("");
      configuredModelEntry = catalog.models.find((candidate) => candidate.provider === provider && candidate.model === model);
      expect(configuredModelEntry, `the installation should configure ${configuredModel}`).toBeDefined();
    }
  }

  const modelAllowed = !configuredModelEntry || allowedModels.some((candidate) => (
    candidate.provider === configuredModelEntry.provider && candidate.model === configuredModelEntry.model
  ));
  const agentAllowed = !configuredBinding || allowedModels.some((candidate) => (
    configuredBinding.models.providers.includes(candidate.provider)
  ));
  if (envelopeName === "basic" && (!modelAllowed || !agentAllowed)) return;
  if (configuredModel) {
    expect(modelAllowed, `${templateId(envelopeName)} should allow the configured model ${configuredModel}`).toBeTruthy();
  }
  if (configuredAgent) {
    expect(agentAllowed, `${templateId(envelopeName)} should allow a model supported by ${configuredAgent}`).toBeTruthy();
  }

  const agentPicker = fieldLocator(page, "Coding agent");
  if (configuredAgent) {
    await expect(agentPicker, "the Run form should expose the configured Coding agent picker").toHaveCount(1);
    await selectConfiguredOption(agentPicker, configuredAgent, "Coding agent");
  }
  const modelPicker = fieldLocator(page, "Model");
  if (configuredModel) {
    if (new URL(page.url()).pathname === "/agents/new") {
      await expect(modelPicker, "the New agent form chooses its model from the selected Envelope").toHaveCount(0);
    } else {
      await expect(modelPicker, "the Run form should expose the configured Model picker").toHaveCount(1);
      await selectConfiguredOption(modelPicker, configuredModel, "Model");
    }
  } else if (await modelPicker.count()) {
    await chooseFirstOfferedModel(page);
  }
}

async function openExecutionStage(page) {
  const jobs = page.getByRole("navigation", { name: "Run jobs" });
  await expect(jobs).toBeVisible();
  await jobs.getByRole("button", { name: /^Agent execution(?:\s|$)/ }).click();
  const stage = page.getByRole("heading", { name: "Agent execution", exact: true }).locator("xpath=ancestor::section[1]");
  await expect(stage).toBeVisible();
  const logStep = stage.locator("details").first();
  if (await logStep.count() && !(await logStep.getAttribute("open"))) await logStep.locator("summary").click();
}

function decimalBelow(value, label) {
  const match = /^(0|[1-9][0-9]*)(?:\.([0-9]{1,6}))?$/.exec(value);
  expect(match, `${label} should use the authority decimal syntax`).toBeTruthy();
  const fractional = match[2] ?? "";
  const scale = 1_000_000n;
  const units = BigInt(match[1]) * scale + BigInt(fractional.padEnd(6, "0") || "0");
  expect(units > 1n, `${label} must allow a lower positive requirement`).toBeTruthy();
  const lower = units / 2n;
  const whole = lower / scale;
  const remainder = (lower % scale).toString().padStart(6, "0").replace(/0+$/, "");
  return remainder ? `${whole}.${remainder}` : whole.toString();
}

function durationBelow(value) {
  const match = /^([1-9][0-9]*)([smh])$/.exec(value);
  expect(match, "the Envelope time-to-live limit should use seconds, minutes, or hours").toBeTruthy();
  const multipliers = { s: 1, m: 60, h: 3_600 };
  const seconds = Number(match[1]) * multipliers[match[2]];
  expect(seconds, "the Envelope time-to-live limit must allow a lower positive duration").toBeGreaterThan(1);
  return `${Math.floor(seconds / 2)}s`;
}

async function fillEnvelopeRequirements(journey, page, boundary) {
  const request = journey.requests.get(journey.currentTemplate);
  expect(request, "an active Envelope should be selected before stating Task requirements").toBeTruthy();
  const spec = structuredClone((request.approvedEnvelope ?? request.requestedEnvelope).spec);
  const atLimit = boundary === "at";
  const requirements = {
    authority: {
      budget: {
        currency: spec.budget.currency,
        monthlyLimit: atLimit ? spec.budget.monthlyLimit : decimalBelow(spec.budget.monthlyLimit, "monthly limit"),
        singleRunLimit: spec.budget.singleRunLimit == null
          ? null
          : atLimit ? spec.budget.singleRunLimit : decimalBelow(spec.budget.singleRunLimit, "limit per run"),
      },
      llms: spec.llms,
      runner: {
        compute: atLimit ? spec.runner?.compute ?? null : null,
        memory: atLimit ? spec.runner?.memory ?? null : null,
        platforms: atLimit ? spec.runner?.platforms ?? [] : [],
        storage: atLimit ? spec.runner?.storage ?? null : null,
      },
      tools: atLimit ? spec.tools : [],
      ttl: atLimit ? spec.ttl : durationBelow(spec.ttl),
      ...(spec.runtimeMinutesLimit == null ? {} : {
        runtimeMinutesLimit: atLimit
          ? spec.runtimeMinutesLimit
          : decimalBelow(spec.runtimeMinutesLimit, "runtime-minutes limit"),
      }),
    },
  };
  journey.statedSingleRunLimit = requirements.authority.budget.singleRunLimit;
  await fieldLocator(page, "Requirements (optional JSON)").fill(JSON.stringify(requirements));
}

async function clickControl(page, name) {
  await page.getByRole("button", { name, exact: true })
    .or(page.getByRole("link", { name, exact: true }))
    .first()
    .click();
}

async function assertDataRows(scope, journey, dataTable, layout = "rows") {
  for (const rawRow of dataTable.raw()) {
    const values = rawRow.map((cell) => resolveText(journey, cell.trim())).filter(Boolean);
    if (values.length === 0) continue;
    if (layout === "columns") {
      const occurrences = new Map();
      for (const value of values) occurrences.set(value, (occurrences.get(value) ?? 0) + 1);
      for (const [value, wanted] of occurrences) {
        await expect.poll(() => scope.getByText(value, { exact: true }).count(), {
          message: `${value} should appear ${wanted} time(s) across the compared columns`,
        }).toBeGreaterThanOrEqual(wanted);
      }
      continue;
    }
    if (values.length === 1) {
      await expect(scope.getByText(values[0], { exact: !values[0].endsWith(":") }).first()).toBeVisible();
      continue;
    }
    const labelledGridRows = scope.locator("strong")
      .filter({ hasText: new RegExp(`^${escapeRegExp(values[0])}$`) })
      .locator("xpath=..");
    const candidates = await labelledGridRows.count()
      ? labelledGridRows
      : scope.locator("tr, [role=row], dl > div, ul > li, ol > li");
    const index = await candidates.evaluateAll((elements, expected) => elements.findIndex((element) => {
      const text = element.innerText.replace(/\s+/g, " ").trim();
      return expected.every((value) => text.includes(value));
    }), values);
    expect(index, `one visible row should contain ${values.join(" | ")}`).toBeGreaterThanOrEqual(0);
    const row = candidates.nth(index);
    await expect(row).toBeVisible();
    for (const value of values) {
      if (layout === "contained-row") await expect(row).toContainText(value);
      else await expect(row.getByText(value, { exact: !value.endsWith(":") }).first()).toBeVisible();
    }
  }
}

Given(/^the optional integration "([^"]+)" is switched (off|on)$/, async ({ journey, page }, integration, state) => {
  expect(["ticket system", "repository integration", "tool connections"]).toContain(integration);
  await page.goto("/");
  await assertSignedIn(page);
  if (integration === "ticket system") {
    const data = await apiJson(page, "/admin/api/v1/requests?limit=1");
    expect(data.decisionChannelEnabled, `the ticket-system decision channel should be ${state}`).toBe(state === "on");
    return;
  }
  if (integration === "repository integration") {
    const data = await apiJson(page, "/app/api/v1/github/repositories");
    expect(Array.isArray(data.repositories), "the repository integration response should be valid").toBeTruthy();
    if (state === "off") {
      expect(data.source, "the optional user repository connection should be off").not.toBe("connection");
      return;
    }
    expect(data.repositories.some((candidate) => `${candidate.owner}/${candidate.name}` === repository && candidate.ready), `${repository} should be ready in the installation`).toBeTruthy();
    return;
  }
  const data = await apiJson(page, "/app/api/v1/connections");
  expect(Array.isArray(data.available) && Array.isArray(data.connections), "the tool-connections response should be valid").toBeTruthy();
  const active = data.connections.filter((connection) => connection.status.phase !== "disconnected");
  journey.integrationStates.set("tool connections", {
    activeForAccount: active.map((connection) => connection.provider),
    enabledByInstallation: data.available.filter((connection) => connection.enabled).map((connection) => connection.provider),
  });
});

Given(/^the installation's configuration has the coding agent "([^"]+)" with the model "([^"]+)"$/, async ({ journey, page }, agent, model) => {
  expect(agent).toMatch(/^<Agent [AB]>$/);
  expect(model).toMatch(/^<provider-[ab]>\/<model-[ab]>$/);
  const [capabilities, catalog] = await Promise.all([
    apiJson(page, "/app/api/v1/capabilities"),
    apiJson(page, "/admin/api/v1/capabilities"),
  ]);
  const index = agent === "<Agent A>" ? 0 : 1;
  const binding = capabilities.agents[index];
  expect(binding, `the installation should configure ${agent}`).toBeDefined();
  const alreadySelected = new Set([...journey.modelAliases.entries()]
    .filter(([alias]) => /^<provider-[ab]>\/<model-[ab]>$/.test(alias))
    .map(([, value]) => value));
  const configuredModel = catalog.models.find((candidate) => binding.models.providers.includes(candidate.provider)
    && !alreadySelected.has(`${candidate.provider}/${candidate.model}`));
  expect(configuredModel, `${agent} should have a configured model`).toBeDefined();
  journey.modelAliases.set(agent, binding.displayName ?? binding.agentRef);
  journey.modelAliases.set(agent === "<Agent A>" ? "<agent-a>" : "<agent-b>", binding.agentRef);
  journey.modelAliases.set(model, `${configuredModel.provider}/${configuredModel.model}`);
});

Given("no template exists", async ({ journey }) => {
  journey.expectNoTemplates = !journey.shared.checkedPrefixes.has(runPrefix);
});

Given("I am signed in as a user who is also an administrator", async ({ journey, page }) => {
  await page.goto("/");
  await assertSignedIn(page);
  await expect(page.getByRole("group", { name: "Workspace view" })).toBeVisible();
  journey.runRuntimeMinutes = await chooseUnusedRuntimeMinutes(page);
  await recordInitialRuns(journey, page);
});

When("I choose {string} in {string}", async ({ journey, page }, choice, field) => {
  if (field === "Workspace view") {
    const workspace = choice === "Admin workspace" ? "admin" : "user";
    await page.getByRole("group", { name: "Workspace view" }).getByRole("button", { name: workspace, exact: true }).click();
    await page.waitForURL((url) => workspace === "admin" ? url.pathname.startsWith("/admin/") : !url.pathname.startsWith("/admin/"));
    await expect(page.getByRole("navigation", { name: "Primary navigation" })).toBeVisible();
    return;
  }
  if (field === "Models" && /^<provider-[ab]>\/<model-[ab]>$/.test(choice)) {
    const picker = fieldLocator(page, "Models");
    await picker.focus();
    const options = page.getByRole("listbox").getByRole("option");
    await expect.poll(() => options.count(), { message: "J1 requires at least two enabled models from different providers" }).toBeGreaterThan(0);
    const labels = (await options.allInnerTexts()).map((text) => text.split("\n")[0].trim());
    const firstProvider = journey.selectedModels[0]?.split("/")[0];
    const index = firstProvider ? labels.findIndex((label) => label.split("/")[0] !== firstProvider) : 0;
    if (index < 0) throw new Error("J1 requires at least two enabled models from different providers.");
    const label = labels[index];
    journey.modelAliases.set(choice, label);
    journey.selectedModels.push(label);
    await options.nth(index).click();
    return;
  }
  const select = field === "Time-to-live limit"
    ? page.getByRole("combobox", { name: "Time-to-live unit" })
    : fieldLocator(page, field);
  if (field === "Template" && choice !== "Custom envelope") {
    journey.currentTemplate = choice;
    await select.selectOption(`template:${templateId(choice)}`);
    return;
  }
  if (field === "Envelope") {
    const request = await requestForTemplate(page, choice, "provisioned");
    expect(request, `a provisioned Envelope from ${templateId(choice)} should exist`).toBeTruthy();
    journey.currentTemplate = choice;
    journey.requests.set(choice, request);
    await select.selectOption(request.id);
    await selectConfiguredRuntime(page, (request.approvedEnvelope ?? request.requestedEnvelope).spec, choice);
    return;
  }
  if (field === "Workspace repository" && choice === "<owner>/<repository>") {
    await select.selectOption({ label: repository });
    return;
  }
  if (field === "Coding agent" && /^<Agent [AB]>$/.test(choice)) {
    await selectConfiguredOption(select, resolveText(journey, choice), "Coding agent");
    return;
  }
  await select.selectOption({ label: resolveText(journey, choice) });
});

Then("I see in {string}:", async ({ journey, page }, area, dataTable) => {
  const scope = areaLocator(page, area);
  await expect(scope).toBeVisible();
  if (area === "Templates") {
    const [expectedHeaders, ...expectedRows] = dataTable.raw();
    const headers = scope.getByRole("columnheader");
    await expect.poll(() => headers.count()).toBeGreaterThanOrEqual(expectedHeaders.length);
    for (const [index, expected] of expectedHeaders.entries()) {
      await expect(headers.nth(index)).toHaveText(resolveText(journey, expected.trim()));
    }
    for (const expectedRow of expectedRows) {
      const firstValue = expectedRow.find((cell) => cell.trim());
      expect(firstValue, "a Templates row must contain at least one expected value").toBeTruthy();
      const row = scope.getByRole("rowgroup").getByRole("link").filter({
        hasText: resolveText(journey, firstValue.trim()),
      });
      await expect(row).toHaveCount(1);
      const cells = row.locator(":scope > span");
      await expect.poll(() => cells.count()).toBeGreaterThanOrEqual(expectedHeaders.length);
      for (const [index, expected] of expectedRow.entries()) {
        const value = resolveText(journey, expected.trim());
        if (value && index === 0) await expect(cells.nth(index)).toContainText(value);
        else if (value) await expect(cells.nth(index)).toHaveText(value);
        else await expect(cells.nth(index)).toHaveText("");
      }
    }
    return;
  }
  if (area === "Needs action") {
    for (const [name] of dataTable.raw()) {
      const request = journey.requests.get(name) ?? await requestForTemplate(page, name);
      expect(request, `a request from ${templateId(name)} should need action`).toBeTruthy();
      journey.requests.set(name, request);
      await expect(scope.getByRole("link", { name: new RegExp(escapeRegExp(request.id)) })).toBeVisible();
    }
    return;
  }
  if (area === "Review exact Task definition") {
    for (const row of dataTable.raw()) {
      for (const cell of row) {
        if (cell.trim()) await expect(scope).toContainText(resolveText(journey, cell.trim()));
      }
    }
    return;
  }
  await assertDataRows(scope, journey, dataTable, area === "Workspace access" ? "columns" : area === "Repository checkout" ? "contained-row" : "rows");
});

When("I open {string}", async ({ journey, page }, destination) => {
  const name = resolveText(journey, destination);
  if (name === "Review exact Task definition") {
    await expect(areaLocator(page, name)).toBeVisible();
    await areaLocator(page, name).scrollIntoViewIfNeeded();
    return;
  }
  if (name === "Repository details") {
    await page.locator("summary").filter({ hasText: name }).first().click();
    return;
  }
  if (name === "Agent execution") {
    await openExecutionStage(page);
    return;
  }
  if (name === "More run actions") {
    await page.getByRole("button", { name, exact: true }).click();
    return;
  }
  const linkName = name === "Get started" ? /^Get started(?:\s+\d+\/\d+)?$/ : name;
  await page.getByRole("navigation", { name: "Primary navigation" }).getByRole("link", { name: linkName, exact: name !== "Get started" }).click();
  if (name === "Templates") {
    await waitForTemplateList(page);
    const initialCount = await prefixedTemplateRows(page).count();
    if (journey.expectNoTemplates) {
      expect(initialCount, `no template with run prefix ${runPrefix} should exist`).toBe(0);
      journey.shared.checkedPrefixes.add(runPrefix);
    }
    journey.initialCounts.set("Templates", initialCount);
  }
  if (name === "Runs" && !journey.initialCounts.has("Runs")) {
    await waitForCollection(page, "Runs", "No runs yet.");
    await recordInitialRuns(journey, page);
    journey.initialCounts.set("Runs", journey.initialRunIds.size);
  }
});

When("I press {string}", async ({ journey, page }, control) => {
  const resolved = resolveText(journey, control);
  if (resolved === "Try a test run") {
    await page.getByRole("listitem")
      .getByRole("button", { name: new RegExp(`^${escapeRegExp(resolved)}(?:\\s|$)`) })
      .click();
    return;
  }
  if (/^Save version \d+$/.test(resolved) || resolved === "Run") {
    const target = resolved.startsWith("Save version") ? /\/tasks\/[^/?#]+$/ : /\/runs\/new\?task=/;
    await Promise.all([page.waitForURL(target), clickControl(page, resolved)]);
    return;
  }
  if (/^Edit into v\d+$/.test(resolved)) {
    await Promise.all([page.waitForURL(/\/tasks\/[^/?#]+\/edit$/), clickControl(page, resolved)]);
    return;
  }
  if (resolved === "Run now" && await page.getByRole("link", { name: resolved, exact: true }).count()) {
    await Promise.all([page.waitForURL(/\/runs\/new/), clickControl(page, resolved)]);
    return;
  }
  if (["Submit request", "Submit for approval"].includes(resolved)) {
    await Promise.all([
      page.waitForURL(envelopeDetailPath, { timeout: 30_000 }),
      clickControl(page, resolved),
    ]);
    const requestId = new URL(page.url()).pathname.split("/").filter(Boolean).at(-1);
    let request = null;
    await expect.poll(async () => {
      request = (await userRequests(page)).find((candidate) => candidate.id === requestId) ?? null;
      return request?.status ?? null;
    }, { message: "the submitted Envelope request should be persisted" }).not.toBeNull();
    if (journey.currentTemplate) journey.requests.set(journey.currentTemplate, request);
    await expect(page.locator("[data-tone]").filter({ hasText: new RegExp(`^${escapeRegExp(request.status)}$`, "i") }).first()).toBeVisible();
    return;
  }
  const transient = resolved === "Run now" ? "Starting…" : null;
  if (!transient) {
    await clickControl(page, resolved);
  } else {
    const label = expect(page.getByRole("button", { name: transient, exact: true }))
      .toBeVisible()
      .then(() => "label", () => null);
    const runPage = page.waitForURL(/\/runs\/(?!new(?:[/?#]|$))[^/?#]+/, { timeout: 30_000 })
      .then(() => "run page", () => null);
    await clickControl(page, resolved);
    const outcome = await Promise.race([label, runPage])
      ?? await Promise.all([label, runPage]).then((results) => results.find(Boolean) ?? null);
    expect(outcome, "pressing Run now should show Starting… or open the Run page").toBeTruthy();
    journey.transientSeen.add(transient);
  }
  if (resolved === "Create template") journey.expectNoTemplates = false;
});

When("I type {string} into {string}", async ({ journey, page }, value, field) => {
  const input = fieldLocator(page, field);
  await input.fill(resolveText(journey, value));
  if (["Eligible member roles", "Models"].includes(field)) await input.press("Enter");
});

When("I leave {string} empty", async ({ page }, field) => {
  await fieldLocator(page, field).fill("");
});

Then("I see {string}", async ({ journey, page }, text) => {
  const expected = resolveText(journey, text);
  if (journey.transientSeen.has(expected)) return;
  if (expected === "Get started") {
    await expect(page.locator('section[aria-label="Get started"]')).toBeVisible();
    return;
  }
  const exact = page.getByText(expected, { exact: true }).first();
  if (await exact.count() && await exact.isVisible()) return;
  const heading = page.getByRole("heading", { name: expected, exact: true }).first();
  if (await heading.count() && await heading.isVisible()) return;
  for (const label of ["Limit per run", "Time-to-live limit"]) {
    if (!expected.startsWith(`${label} `)) continue;
    const value = expected.slice(label.length + 1);
    const term = page.locator("dt").filter({ hasText: new RegExp(`^${escapeRegExp(label)}$`) }).first();
    await expect(term).toBeVisible();
    await expect(term.locator("xpath=following-sibling::dd[1]")).toHaveText(value);
    return;
  }
  await expect(exact).toBeVisible();
});

Then("I see the text fragment {string}", async ({ journey, page }, text) => {
  await expect(page.getByText(resolveText(journey, text), { exact: false }).first()).toBeVisible();
});

Then("I do not see {string}", async ({ journey, page }, text) => {
  await expect(page.getByText(resolveText(journey, text), { exact: false })).toHaveCount(0);
});

Then("I do not see {string} in {string}", async ({ journey, page }, text, area) => {
  const expected = resolveText(journey, text);
  const scope = fieldLocator(page, area);
  if (await scope.count()) {
    await expect(scope.locator("option").filter({ hasText: expected })).toHaveCount(0);
    return;
  }
  await expect(areaLocator(page, area).getByText(expected, { exact: false })).toHaveCount(0);
});

Then("{string} is disabled", async ({ journey, page }, field) => {
  if (/^<Agent [AB]>$/.test(field)) {
    const expected = resolveText(journey, field);
    const picker = fieldLocator(page, "Coding agent");
    const option = picker.locator("option").filter({ hasText: expected });
    await expect(option).toHaveJSProperty("disabled", true);
    return;
  }
  await expect(fieldLocator(page, field)).toBeDisabled();
});

Then("the disabled agent {string} says {string}", async ({ journey, page }, agent, text) => {
  const option = fieldLocator(page, "Coding agent").locator("option").filter({ hasText: resolveText(journey, agent) });
  await expect(option).toHaveJSProperty("disabled", true);
  await expect(option).toContainText(resolveText(journey, text));
});

When("I tick {string}", async ({ page }, field) => {
  await checkboxLocator(page, field).check();
});

When("I untick {string}", async ({ page }, field) => {
  await checkboxLocator(page, field).uncheck();
});

When("I open {string} in {string}", async ({ journey, page }, item, area) => {
  const scope = areaLocator(page, area);
  if (["Envelopes", "Needs action"].includes(area) && item !== "Custom envelope") {
    const request = journey.requests.get(item) ?? await requestForTemplate(page, item);
    expect(request, `an Envelope request from ${templateId(item)} should exist`).toBeTruthy();
    journey.requests.set(item, request);
    await scope.getByRole("link", { name: new RegExp(escapeRegExp(request.id)) }).click();
    return;
  }
  if (area === "Tasks") {
    const task = scope.getByRole("article").filter({ hasText: resolveText(journey, item) });
    await expect(task).toHaveCount(1);
    await task.getByRole("link", { name: "Review", exact: true }).click();
    return;
  }
  await scope.getByRole("link").filter({ hasText: resolveText(journey, item) }).click();
});

Then("the field {string} shows {string}", async ({ journey, page }, field, value) => {
  const expected = resolveText(journey, value);
  if (field === "Workspace (advanced JSON array)") {
    await expect.poll(async () => {
      try {
        return JSON.stringify(JSON.parse(await fieldLocator(page, field).inputValue()));
      } catch {
        return "invalid";
      }
    }).toBe(JSON.stringify(JSON.parse(expected)));
    return;
  }
  const locator = fieldLocator(page, field);
  const path = new URL(page.url()).pathname;
  const readOnlyTask = /^\/tasks\/(?!new$)[^/]+$/.test(path);
  if (readOnlyTask) {
    const term = page.locator("dt").filter({ hasText: new RegExp(`^${escapeRegExp(field)}$`) }).first();
    await expect(term).toBeVisible();
    await expect(term.locator("xpath=following-sibling::dd[1]")).toContainText(expected);
    return;
  }
  await expect(locator).toBeVisible();
  if (await locator.evaluate((element) => element.tagName === "SELECT")) {
    await expect(locator.locator("option:checked")).toContainText(expected);
    return;
  }
  await expect(locator).toHaveValue(expected);
});

When("I state requirements that are all below the Envelope's limits", async ({ journey, page }) => {
  await fillEnvelopeRequirements(journey, page, "below");
});

When("I state requirements that are all at the Envelope's limits", async ({ journey, page }) => {
  await fillEnvelopeRequirements(journey, page, "at");
});

When("I state a per-run budget of zero", async ({ journey, page }) => {
  await fillEnvelopeRequirements(journey, page, "below");
  const field = fieldLocator(page, "Requirements (optional JSON)");
  const requirements = JSON.parse(await field.inputValue());
  requirements.authority.budget.singleRunLimit = "0";
  journey.statedSingleRunLimit = "0";
  await field.fill(JSON.stringify(requirements));
});

Given("the installation keeps execution logs", async ({ page }) => {
  await page.goto("/runs/new");
  await expect(checkboxLocator(page, "Capture execution log")).toBeEnabled();
});

Then("the log holds every line printed before the run ended", async ({ journey, page }) => {
  const taskUid = journey.run?.taskUid;
  expect(taskUid, "a completed run should be recorded before checking its log").toBeTruthy();
  expect(journey.run.finalized, "the run should be terminal before checking its complete log").toBeTruthy();
  const response = await page.request.get(new URL(`/app/api/v1/runs/${encodeURIComponent(taskUid)}/logs/stdout`, page.url()).toString());
  expect(response.ok(), "the terminal run's stdout log should be available").toBeTruthy();
  const content = await response.text();
  expect(content, "the completed run should have captured stdout").not.toBe("");
  const visible = page.getByLabel("stdout log lines");
  await expect(visible).toBeVisible();
  await expect(visible).toContainText(content.trim());
});

Then("the drop-down {string} offers {string}", async ({ journey, page }, field, offered) => {
  const expected = resolveText(journey, offered);
  const picker = page.getByRole("combobox", { name: field, exact: true });
  const option = picker.locator("option").filter({ hasText: expected }).first();
  await expect(option, `${field} should offer text containing ${expected}`).toBeAttached();
  if (field === "Template" && expected === "Custom envelope") {
    await expect(picker.locator("option").last()).toHaveText(expected);
  }
});

When("I type a value the installation does not accept into {string}", async ({ page }, field) => {
  await fieldLocator(page, field).fill("not valid");
});

Then(/^the page "([^"]+)" lists (\d+) new (?:entry|entries)$/, async ({ journey, page }, area, expectedText) => {
  const expected = Number(expectedText);
  await page.getByRole("navigation", { name: "Primary navigation" }).getByRole("link", { name: area, exact: true }).click();
  const initial = journey.initialCounts.get(area);
  expect(initial, `${area} must record its starting count before comparing new entries`).toBeDefined();
  if (area === "Templates") {
    await waitForTemplateList(page);
    await expect.poll(() => prefixedTemplateRows(page).count(), {
      message: `${area} should list ${expected} new run-prefixed entries`,
    }).toBe(initial + expected);
    return;
  }
  if (area === "Runs") {
    expect(journey.initialRunIds, "Runs must record its starting identities before comparing new entries").toBeTruthy();
    let newRuns = [];
    await expect.poll(async () => {
      newRuns = (await userRuns(page)).filter((run) => !journey.initialRunIds.has(run.taskUid));
      return newRuns.length;
    }, { message: `${area} should list ${expected} new entries` }).toBe(expected);
    for (const run of newRuns) {
      await expect(page.getByRole("table", { name: "Runs" }).getByRole("link", { name: new RegExp(escapeRegExp(run.taskUid)) })).toBeVisible();
    }
    return;
  }
  const count = area === "Envelopes"
    ? async () => {
        await waitForCollection(page, "Envelopes", "No envelopes yet.");
        return page.getByRole("table", { name: "Envelopes" }).getByRole("rowgroup").getByRole("link").filter({ hasText: runPrefix }).count();
      }
    : null;
  expect(count, `${area} should have a supported collection counter`).toBeTruthy();
  await expect.poll(count, { message: `${area} should list ${expected} new entries` }).toBe(initial + expected);
});

Then("the message names {string}", async ({ page }, field) => {
  const label = fieldLocator(page, field);
  await expect(label).toBeVisible();
  await expect(label.locator("xpath=ancestor::label[1]").getByRole("alert")).toBeVisible();
});

Given("the template {string} exists", async ({ page }, name) => {
  const data = await apiJson(page, "/app/api/v1/envelope-templates");
  const template = data.templates.find((candidate) => candidate.id === templateId(name));
  expect(template, `template ${templateId(name)} should exist in the installation`).toBeDefined();
});

Given("I am signed in as a user with the role {string}", async ({ journey, page }, role) => {
  expect(resolveText({ modelAliases: new Map() }, role)).toBe(memberRole);
  await page.goto("/");
  await assertSignedIn(page);
  journey.runRuntimeMinutes = await chooseUnusedRuntimeMinutes(page);
  await recordInitialRuns(journey, page);
});

Given("the Get started guide is visible", async ({ journey, page }) => {
  const preferences = await apiJson(page, "/app/api/v1/preferences");
  expect(typeof preferences.onboardingDismissed, "the onboarding dismissal preference should be a boolean").toBe("boolean");
  const originalOnboardingDismissed = preferences.onboardingDismissed;
  journey.restoreOnboardingDismissal = async () => {
    const current = await apiJson(page, "/app/api/v1/preferences");
    if (current.onboardingDismissed !== originalOnboardingDismissed) {
      await mutationJson(page, "/app/api/v1/preferences", "PUT", { onboardingDismissed: originalOnboardingDismissed });
    }
    const restored = await apiJson(page, "/app/api/v1/preferences");
    expect(restored.onboardingDismissed, "the onboarding dismissal preference should be restored after the scenario").toBe(originalOnboardingDismissed);
  };
  if (preferences.onboardingDismissed) {
    await mutationJson(page, "/app/api/v1/preferences", "PUT", { onboardingDismissed: false });
    await page.reload();
  }
  const guide = page.locator('section[aria-label="Get started"]');
  await expect(guide, "the signed-in account should have an incomplete Get started guide").toBeVisible();
  journey.onboardingProgressLabel = await guide.getByRole("progressbar").getAttribute("aria-label");
  expect(journey.onboardingProgressLabel, "the Get started guide should expose its current progress").toBeTruthy();
});

Then("the Get started guide and sidebar entry are hidden", async ({ page }) => {
  await expect.poll(async () => (await apiJson(page, "/app/api/v1/preferences")).onboardingDismissed, {
    message: "hiding Get started should persist the dismissal preference",
  }).toBe(true);
  await expect(page.locator('section[aria-label="Get started"]')).toHaveCount(0);
  await expect(page.getByRole("navigation", { name: "Primary navigation" }).getByRole("link", { name: /^Get started/ })).toHaveCount(0);
});

Then("the Get started guide and sidebar entry are visible with the same progress", async ({ journey, page }) => {
  await expect.poll(async () => (await apiJson(page, "/app/api/v1/preferences")).onboardingDismissed, {
    message: "reopening Get started should persist the visible preference",
  }).toBe(false);
  const guide = page.locator('section[aria-label="Get started"]');
  await expect(guide).toBeVisible();
  await expect(page.getByRole("navigation", { name: "Primary navigation" }).getByRole("link", { name: /^Get started/ })).toBeVisible();
  await expect(guide.getByRole("progressbar")).toHaveAttribute("aria-label", journey.onboardingProgressLabel);
});

Given("I hold no Envelope", async ({ journey, page }) => {
  const count = (await userRequests(page)).filter((request) => request.templateId?.startsWith(`${runPrefix}-`)).length;
  expect(count, `run prefix ${runPrefix} should not already have an Envelope request`).toBe(0);
  journey.initialCounts.set("Envelopes", count);
});

Given("I hold an Envelope from the template {string}", async ({ journey, page }, name) => {
  if (journey.jira && name === "jira-read") await ensureExtendedTemplate(page, name);
  let request = await requestForTemplate(page, name, "provisioned");
  if (!request && name === "standard") {
    const template = (await userTemplates(page)).find((candidate) => candidate.id === templateId(name));
    expect(template, `template ${templateId(name)} should exist in the installation`).toBeDefined();
    request = await provisionEnvelope(journey, page, template);
  }
  if (!request && (journey.extended || journey.jira)) request = await provisionExtendedEnvelope(journey, page, name);
  expect(request, `a provisioned Envelope from ${templateId(name)} should exist`).toBeTruthy();
  journey.currentTemplate = name;
  journey.requests.set(name, request);
});

Given("the repository {string} is allowed in this installation", async ({ journey, page }, name) => {
  const expected = resolveText({ modelAliases: new Map() }, name);
  const data = await apiJson(page, "/app/api/v1/github/repositories");
  const installed = data.repositories.find((candidate) => `${candidate.owner}/${candidate.name}` === expected);
  expect(installed, `${expected} should be offered by the repository integration`).toBeDefined();
  expect(installed.ready, `${expected} should be admitted and ready`).toBeTruthy();
  journey.repository = installed;
});

Then("I see the status {string}", async ({ journey, page }, status) => {
  if (new URL(page.url()).pathname === "/envelopes/new") {
    await page.waitForURL(envelopeDetailPath, { timeout: 30_000 });
  }
  if (/^\/envelopes\/[A-Za-z0-9-]+/.test(new URL(page.url()).pathname)) {
    const requestId = new URL(page.url()).pathname.split("/").filter(Boolean).at(-1);
    if (journey.currentTemplate && requestId) {
      const request = (await userRequests(page)).find((candidate) => candidate.id === requestId);
      if (request) journey.requests.set(journey.currentTemplate, request);
    }
  }
  let badge = page.locator("[data-tone]").filter({ hasText: new RegExp(`^${escapeRegExp(status)}$`, "i") }).first();
  try {
    await expect(badge).toBeVisible({ timeout: 3_000 });
  } catch (error) {
    if (status !== "approved" || !(await page.getByText("The approval was applied.", { exact: true }).count())) throw error;
    await page.reload();
    badge = page.locator("[data-tone]").filter({ hasText: new RegExp(`^${escapeRegExp(status)}$`, "i") }).first();
    await expect(badge).toBeVisible();
  }
});

Then("the field {string} has a warning outline", async ({ page }, field) => {
  await expect(fieldLocator(page, field)).toHaveAttribute("aria-invalid", "true");
});

Then("{string} is enabled", async ({ page }, control) => {
  const button = page.getByRole("button", { name: control, exact: true });
  if (await button.count()) {
    await expect(button).toBeEnabled();
    return;
  }
  await expect(fieldLocator(page, control)).toBeEnabled();
});

Then("the run ends as {string} within {int} min", async ({ journey, page }, phase, minutes) => {
  await expect(page).toHaveURL(/\/runs\/(?!new(?:[/?#]|$))[^/?#]+/, { timeout: 30_000 });
  const taskUid = new URL(page.url()).pathname.split("/").filter(Boolean).at(-1);
  expect(taskUid, "the run detail address should include the Task UID").toBeTruthy();
  const deadline = Date.now() + minutes * 60_000;
  while (true) {
    const data = await apiJson(page, `/app/api/v1/runs/${encodeURIComponent(taskUid)}`);
    journey.run = data.run;
    if (data.run.finalized) {
      if (data.run.phase !== phase) {
        const failure = `the run should end as ${phase}; actual phase=${data.run.phase}; category=${data.run.errorCategory ?? "none"}; reason=${data.run.failureReason ?? "none"}`;
        if (isIssue414RunFailure({
          knownIssue: journey.knownIssue,
          statedSingleRunLimit: journey.statedSingleRunLimit,
          phase: data.run.phase,
          finalized: data.run.finalized,
          failureReason: data.run.failureReason,
          atDeadline: false,
        })) {
          throw new Error(`[steward-known-failure:#414] ${failure}`);
        }
        throw new Error(failure);
      }
      break;
    }
    const atDeadline = Date.now() >= deadline;
    if (isIssue414RunFailure({
      knownIssue: journey.knownIssue,
      statedSingleRunLimit: journey.statedSingleRunLimit,
      phase: data.run.phase,
      finalized: data.run.finalized,
      failureReason: data.run.failureReason,
      atDeadline,
    })) {
      throw new Error(`[steward-known-failure:#414] the run did not finish within ${minutes} min; phase=${data.run.phase}; finalized=false; reason=none`);
    }
    expect(Date.now(), `the run should end as ${phase} within ${minutes} min`).toBeLessThan(deadline);
    await page.waitForTimeout(2_000);
  }
  const selectedRequest = journey.currentTemplate ? journey.requests.get(journey.currentTemplate) : null;
  if (selectedRequest) {
    expect(selectedRequest.envelopeInstanceId, `the ${templateId(journey.currentTemplate)} request should identify its provisioned Envelope`).toBeTruthy();
    expect(journey.run.userEnvelopeInstanceId, "the run should use the Envelope selected for this scenario").toBe(selectedRequest.envelopeInstanceId);
    expect(journey.run.userEnvelopeRevision, "the run should record the revision of the selected Envelope").toBe(
      (selectedRequest.approvedEnvelope ?? selectedRequest.requestedEnvelope).revision,
    );
  }
  await page.reload();
  await expect(page.locator("[data-tone]").filter({ hasText: new RegExp(`^${escapeRegExp(phase)}$`, "i") }).first()).toBeVisible();
});

Then("the run output contains {string}", async ({ journey, page }, expected) => {
  expect(journey.run?.taskUid, "a completed run should be recorded before checking output").toBeTruthy();
  const outputs = await apiJson(page, `/app/api/v1/runs/${encodeURIComponent(journey.run.taskUid)}/outputs`);
  if (journey.knownIssue === "@known-failing-464" && outputs.files.length === 0) {
    throw new Error("[steward-known-failure:#464] The run succeeded without publishing any output: the agent did not find the repository checkout.");
  }
  expect(outputs.files.length, "the run should publish at least one output file").toBeGreaterThan(0);
  const contents = [];
  for (const file of outputs.files) {
    const response = await page.request.get(new URL(file.downloadUrl, page.url()).toString());
    expect(response.ok(), `run output ${file.path} should be downloadable`).toBeTruthy();
    contents.push(await response.text());
  }
  const output = contents.join("\n").toLocaleLowerCase("en-US");
  const expectedOutput = expected.toLocaleLowerCase("en-US");
  if (journey.knownIssue === "@known-failing-464" && !output.includes(expectedOutput)) {
    throw new Error("[steward-known-failure:#464] The run succeeded without the agent finding the repository checkout.");
  }
  expect(output).toContain(expectedOutput);
});

Then("the run records coding agent {string} and model {string}", async ({ journey, page }, agent, model) => {
  expect(journey.run?.taskUid, "a completed run should be recorded before checking its execution choice").toBeTruthy();
  const expectedAgent = resolveText(journey, agent);
  const [expectedProvider, expectedModel] = resolveText(journey, model).split("/");
  expect(expectedProvider && expectedModel, "the expected model should use provider/model form").toBeTruthy();
  expect(journey.run.codingAgentRuntime, "the run record should keep the chosen coding agent").toBe(expectedAgent);
  const response = await apiJson(page, `/app/api/v1/runs/${encodeURIComponent(journey.run.taskUid)}/package`);
  const packagePath = journey.run.package?.path;
  expect(packagePath, "the completed inline run should identify its package definition").toBeTruthy();
  const definitionText = response.files?.[packagePath];
  expect(definitionText, `the completed run package should contain ${packagePath}`).toBeTruthy();
  const definition = JSON.parse(definitionText);
  expect(definition.runtime?.agentRef, "the run package should keep the chosen coding agent").toBe(expectedAgent);
  expect(definition.runtime?.model, "the run package should keep the chosen model").toEqual({
    provider: expectedProvider,
    model: expectedModel,
  });
});

Then(/^the spend of the run is above ([0-9]+(?:\.[0-9]+)?) ([A-Z]{3})$/, async ({ journey, page }, minimumText, unit) => {
  const minimum = Number(minimumText);
  expect(journey.run?.observedSpend?.currency).toBe(unit);
  expect(Number(journey.run.observedSpend.observedAmount)).toBeGreaterThan(minimum);
  const spend = page.locator("#workspace p").filter({ hasText: /^Spend$/ }).first().locator("xpath=following-sibling::p[1]");
  await expect(spend).toHaveText(new RegExp(`^(?:< 0\\.01|\\d+(?:\\.\\d+)?) ${escapeRegExp(unit)}$`));
  const displayed = await spend.innerText();
  if (displayed.startsWith("< ")) {
    expect(minimum, "a below-one-cent display can only prove a threshold below one cent").toBeLessThan(0.01);
  } else {
    expect(Number(displayed.split(/\s+/)[0]), "the Run page should show spend above the expected minimum").toBeGreaterThan(minimum);
  }
});

Then("every amount of money on the page has {int} decimals and the unit {string}", async ({ page }, decimals, unit) => {
  const amounts = (await page.locator("#workspace").innerText()).match(new RegExp(`(?:< )?\\d+(?:\\.\\d+)? ${escapeRegExp(unit)}\\b`, "g")) ?? [];
  expect(amounts.length, `the page should show at least one amount in ${unit}`).toBeGreaterThan(0);
  for (const amount of amounts) {
    expect(amount).toMatch(new RegExp(`^(?:< )?\\d+\\.\\d{${decimals}} ${escapeRegExp(unit)}$`));
  }
});

Then("the Envelope from the template {string} shows the same spend as used out of its monthly limit", async ({ journey, page }, name) => {
  expect(journey.run?.observedSpend, "the completed run should report spend").toBeTruthy();
  const request = await requestForTemplate(page, name, "provisioned");
  expect(request?.usage?.spend, `${templateId(name)} should report monthly spend`).toBeTruthy();
  expect(request.usage.spend.observed).toBe(journey.run.observedSpend.observedAmount);
  const observed = Number(request.usage.spend.observed);
  const formatted = observed > 0 && observed < 0.01
    ? `< 0.01 ${request.usage.spend.currency}`
    : `${observed.toFixed(2)} ${request.usage.spend.currency}`;
  const row = page.getByRole("table", { name: "Envelopes" }).getByRole("link", { name: new RegExp(escapeRegExp(request.id)) });
  await expect(row.getByText(formatted, { exact: true })).toBeVisible();
});

When("I start a Hello world run with the Envelope from the template {string}", async ({ journey, page }, name) => {
  await page.goto("/runs/new");
  const request = await requestForTemplate(page, name, "provisioned");
  expect(request, `a provisioned Envelope from ${templateId(name)} should exist`).toBeTruthy();
  journey.currentTemplate = name;
  journey.requests.set(name, request);
  await fieldLocator(page, "Envelope").selectOption(request.id);
  await selectConfiguredRuntime(page, (request.approvedEnvelope ?? request.requestedEnvelope).spec, name);
  const source = fieldLocator(page, "Package source");
  await expect(source, "the direct Run form should expose Package source").toHaveCount(1);
  await source.selectOption({ label: "Inline package" });
  await page.getByRole("button", { name: "Run now", exact: true }).click();
});

Then("{string} shows a commit of {int} hex characters that is the head of the branch {string}", async ({ journey, page }, field, length, branch) => {
  const term = page.locator("dt").filter({ hasText: new RegExp(`^${escapeRegExp(field)}$`) }).first();
  const value = term.locator("xpath=following-sibling::dd[1]");
  // The Run page may identify the digest algorithm before the hash (#479).
  await expect(value).toHaveText(new RegExp(`^(?:git:sha1:)?[0-9a-f]{${length}}$`, "i"));
  const exactCommit = (await value.innerText()).trim().replace(/^git:sha1:/, "");
  expect(journey.repository, "the admitted repository should be recorded before checking its branch head").toBeTruthy();
  expect(journey.repository.defaultBranch, "the selected branch should be the repository's default branch").toBe(branch);
  const repositoryUrl = new URL(journey.repository.url);
  const path = `/repos/${encodeURIComponent(journey.repository.owner)}/${encodeURIComponent(journey.repository.name)}/commits/${encodeURIComponent(branch)}`;
  const apiUrl = repositoryUrl.hostname === "github.com"
    ? new URL(path, "https://api.github.com")
    : new URL(`/api/v3${path}`, repositoryUrl.origin);
  const response = await page.request.get(apiUrl.toString(), { headers: { Accept: "application/vnd.github+json" } });
  if (!response.ok() && journey.repository.private) {
    console.log(`Branch-head comparison skipped: the Git host returned ${response.status()} for the private repository.`);
    return;
  }
  expect(response.ok(), `${journey.repository.url} must expose ${branch} so its head can be verified`).toBeTruthy();
  const head = await response.json();
  expect(head.sha, `the run should pin the head of ${branch}`).toBe(exactCommit);
});
