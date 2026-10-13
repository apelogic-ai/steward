import { expect } from "@playwright/test";

import { Given, Then, When } from "./fixtures.mjs";

const runPrefix = process.env.STEWARD_JOURNEY_RUN_PREFIX;
const memberRole = process.env.STEWARD_JOURNEY_MEMBER_ROLE;
const repository = process.env.STEWARD_JOURNEY_REPOSITORY;
const configuredAgent = process.env.STEWARD_JOURNEY_AGENT;
const configuredModel = process.env.STEWARD_JOURNEY_MODEL;

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
  const prefixed = text
    .replaceAll("<run-prefix>", runPrefix)
    .replaceAll("<owner>/<repository>", repository);
  if (prefixed === "member") return memberRole;
  if (prefixed === "Read the repository") return `task-${runPrefix}-repo`;
  if (prefixed === "<run-runtime-minutes>") {
    if (!journey.runRuntimeMinutes) throw new Error("the journey must choose an unused runtime-minutes value before filling the request");
    return journey.runRuntimeMinutes;
  }
  return journey.modelAliases.get(prefixed) ?? prefixed;
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
  if (["Branch or tag", "Display name", "Expires at (optional)", "Name", "New template ID", "Package path", "Prompt", "Rationale", "Template ID", "Total size limit (optional)", "Workspace (advanced JSON array)"].includes(field)) {
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

async function selectConfiguredRuntime(page) {
  const agentPicker = fieldLocator(page, "Coding agent");
  if (configuredAgent && await agentPicker.count()) {
    await selectConfiguredOption(agentPicker, configuredAgent, "Coding agent");
  }
  const modelPicker = fieldLocator(page, "Model");
  if (configuredModel && await modelPicker.count()) {
    await selectConfiguredOption(modelPicker, configuredModel, "Model");
  } else if (await modelPicker.count()) {
    await chooseFirstOfferedModel(page);
  }
}

async function clickControl(page, name) {
  const button = page.getByRole("button", { name, exact: true });
  if (await button.count()) {
    await button.click();
    return;
  }
  await page.getByRole("link", { name, exact: true }).click();
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
  const configuredModel = catalog.models.find((candidate) => binding.models.providers.includes(candidate.provider));
  expect(configuredModel, `${agent} should have a configured model`).toBeDefined();
  journey.modelAliases.set(agent, binding.displayName ?? binding.agentRef);
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
    await selectConfiguredRuntime(page);
    return;
  }
  if (field === "Workspace repository" && choice === "<owner>/<repository>") {
    await select.selectOption({ label: repository });
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
        if (value) await expect(cells.nth(index)).toContainText(value);
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
  for (const row of dataTable.raw()) {
    for (const cell of row) {
      if (!cell.trim()) continue;
      const expected = resolveText(journey, cell.trim());
      await expect(scope.getByText(expected, { exact: !expected.endsWith(":") }).first()).toBeVisible();
    }
  }
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
  await page.getByRole("navigation", { name: "Primary navigation" }).getByRole("link", { name, exact: true }).click();
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
    journey.initialCounts.set("Runs", await page.getByRole("table", { name: "Runs" }).getByRole("rowgroup").getByRole("link").count());
  }
});

When("I press {string}", async ({ journey, page }, control) => {
  const resolved = resolveText(journey, control);
  if (["Save version 1", "Run"].includes(resolved)) {
    const target = resolved === "Save version 1" ? /\/tasks\/[^/?#]+$/ : /\/runs\/new\?task=/;
    await Promise.all([page.waitForURL(target), clickControl(page, resolved)]);
    return;
  }
  if (resolved === "Run now" && await page.getByRole("link", { name: resolved, exact: true }).count()) {
    await Promise.all([page.waitForURL(/\/runs\/new/), clickControl(page, resolved)]);
    return;
  }
  const transient = ["Submit request", "Submit for approval"].includes(resolved)
    ? "Submitting…"
    : resolved === "Run now"
      ? "Starting…"
      : null;
  const observation = transient
    ? expect(page.getByRole("button", { name: transient, exact: true })).toBeVisible()
    : null;
  await clickControl(page, resolved);
  if (observation) {
    await observation;
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

Then("I do not see {string}", async ({ journey, page }, text) => {
  await expect(page.getByText(resolveText(journey, text), { exact: false })).toHaveCount(0);
});

Then("{string} is disabled", async ({ page }, field) => {
  await expect(fieldLocator(page, field)).toBeDisabled();
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
  await expect(fieldLocator(page, field)).toHaveValue(expected);
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
  const count = area === "Envelopes"
    ? async () => {
        await waitForCollection(page, "Envelopes", "No envelopes yet.");
        return page.getByRole("table", { name: "Envelopes" }).getByRole("rowgroup").getByRole("link").filter({ hasText: runPrefix }).count();
      }
    : async () => {
        await waitForCollection(page, "Runs", "No runs yet.");
        return page.getByRole("table", { name: "Runs" }).getByRole("rowgroup").getByRole("link").count();
      };
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
});

Given("I hold no Envelope", async ({ journey, page }) => {
  const count = (await userRequests(page)).filter((request) => request.templateId?.startsWith(`${runPrefix}-`)).length;
  expect(count, `run prefix ${runPrefix} should not already have an Envelope request`).toBe(0);
  journey.initialCounts.set("Envelopes", count);
});

Given("I hold an Envelope from the template {string}", async ({ journey, page }, name) => {
  const request = await requestForTemplate(page, name, "provisioned");
  expect(request, `a provisioned Envelope from ${templateId(name)} should exist`).toBeTruthy();
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
    await page.waitForURL(/\/envelopes\/[A-Za-z0-9-]+$/, { timeout: 30_000 });
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
      expect(data.run.phase, `the run should end as ${phase}`).toBe(phase);
      break;
    }
    expect(Date.now(), `the run should end as ${phase} within ${minutes} min`).toBeLessThan(deadline);
    await page.waitForTimeout(2_000);
  }
  await page.reload();
  await expect(page.locator("[data-tone]").filter({ hasText: new RegExp(`^${escapeRegExp(phase)}$`, "i") }).first()).toBeVisible();
});

Then("the run output contains {string}", async ({ journey, page }, expected) => {
  expect(journey.run?.taskUid, "a completed run should be recorded before checking output").toBeTruthy();
  const outputs = await apiJson(page, `/app/api/v1/runs/${encodeURIComponent(journey.run.taskUid)}/outputs`);
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

Then("the spend of the run is above {float} USD", async ({ journey, page }, minimum) => {
  expect(journey.run?.observedSpend?.currency).toBe("USD");
  expect(Number(journey.run.observedSpend.observedAmount)).toBeGreaterThan(minimum);
  const spend = page.locator("#workspace dt").filter({ hasText: /^Spend$/ }).first().locator("xpath=following-sibling::dd[1]");
  await expect(spend).toHaveText(/^\d+(?:\.\d+)? USD$/);
  expect(Number((await spend.innerText()).split(/\s+/)[0]), "the Run page should show spend above the expected minimum").toBeGreaterThan(minimum);
});

Then("every amount of money on the page has {int} decimals and the unit {string}", async ({ page }, decimals, unit) => {
  const amounts = (await page.locator("#workspace").innerText()).match(new RegExp(`\\b\\d+(?:\\.\\d+)? ${escapeRegExp(unit)}\\b`, "g")) ?? [];
  expect(amounts.length, `the page should show at least one amount in ${unit}`).toBeGreaterThan(0);
  for (const amount of amounts) {
    expect(amount).toMatch(new RegExp(`\\.\\d{${decimals}} ${escapeRegExp(unit)}$`));
  }
});

Then("the Envelope from the template {string} shows the same spend as used out of its monthly limit", async ({ journey, page }, name) => {
  expect(journey.run?.observedSpend, "the completed run should report spend").toBeTruthy();
  const request = await requestForTemplate(page, name, "provisioned");
  expect(request?.usage?.spend, `${templateId(name)} should report monthly spend`).toBeTruthy();
  expect(request.usage.spend.observed).toBe(journey.run.observedSpend.observedAmount);
  const formatted = `${Number(request.usage.spend.observed).toFixed(2)} ${request.usage.spend.currency}`;
  const row = page.getByRole("table", { name: "Envelopes" }).getByRole("link", { name: new RegExp(escapeRegExp(request.id)) });
  await expect(row.getByText(formatted, { exact: true })).toBeVisible();
});

When("I start a Hello world run with the Envelope from the template {string}", async ({ journey, page }, name) => {
  await page.goto("/runs/new");
  const request = await requestForTemplate(page, name, "provisioned");
  expect(request, `a provisioned Envelope from ${templateId(name)} should exist`).toBeTruthy();
  journey.requests.set(name, request);
  await fieldLocator(page, "Envelope").selectOption(request.id);
  await selectConfiguredRuntime(page);
  const source = fieldLocator(page, "Package source");
  if (await source.count()) await source.selectOption({ label: "Inline package" });
  await page.getByRole("button", { name: "Run now", exact: true }).click();
});

Then("{string} shows a commit of {int} hex characters that is the head of the branch {string}", async ({ journey, page }, field, length, branch) => {
  const term = page.locator("dt").filter({ hasText: new RegExp(`^${escapeRegExp(field)}$`) }).first();
  const value = term.locator("xpath=following-sibling::dd[1]");
  await expect(value).toHaveText(new RegExp(`^[0-9a-f]{${length}}$`, "i"));
  const exactCommit = (await value.innerText()).trim();
  expect(journey.repository, "the admitted repository should be recorded before checking its branch head").toBeTruthy();
  expect(journey.repository.defaultBranch, "the selected branch should be the repository's default branch").toBe(branch);
  const repositoryUrl = new URL(journey.repository.url);
  const path = `/repos/${encodeURIComponent(journey.repository.owner)}/${encodeURIComponent(journey.repository.name)}/commits/${encodeURIComponent(branch)}`;
  const apiUrl = repositoryUrl.hostname === "github.com"
    ? new URL(path, "https://api.github.com")
    : new URL(`/api/v3${path}`, repositoryUrl.origin);
  const response = await page.request.get(apiUrl.toString(), { headers: { Accept: "application/vnd.github+json" } });
  expect(response.ok(), `${journey.repository.url} must expose ${branch} so its head can be verified`).toBeTruthy();
  const head = await response.json();
  expect(head.sha, `the run should pin the head of ${branch}`).toBe(exactCommit);
});
