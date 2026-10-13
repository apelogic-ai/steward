import { expect } from "@playwright/test";

import { Given, Then, When } from "./fixtures.mjs";

const runPrefix = process.env.STEWARD_JOURNEY_RUN_PREFIX;

function resolveText(journey, text) {
  const prefixed = text.replaceAll("<run-prefix>", runPrefix);
  return journey.modelAliases.get(prefixed) ?? prefixed;
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function fieldLocator(page, field) {
  const name = new RegExp(`^${escapeRegExp(field)}(?:\\s|$)`, "i");
  if (["Eligible member roles", "Models", "Template"].includes(field)) {
    return page.getByRole("combobox", { name }).first();
  }
  if (["Limit per run", "Monthly limit", "Time-to-live limit", "Runtime limit per month"].includes(field)) {
    return page.getByRole("spinbutton", { name }).first();
  }
  return page.getByLabel(name).first();
}

function checkboxLocator(page, field) {
  return page.getByRole("checkbox", { name: new RegExp(`^${escapeRegExp(field)}$`, "i") });
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
    case "Limit per run":
    case "Monthly limit":
      return fieldLocator(page, area).locator("xpath=ancestor::label[1]");
    case "Tools":
    case "Workspace access":
      return page.getByRole("group", { name: area, exact: true }).first();
    default:
      return page.locator("main");
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

Given(/^the optional integration "([^"]+)" is switched (off|on)$/, async ({}, integration, state) => {
  expect(["ticket system", "repository integration", "tool connections"]).toContain(integration);
  expect(state).toBe("off");
});

Given(/^the installation's configuration has the coding agent "([^"]+)" with the model "([^"]+)"$/, async ({ journey }, agent, model) => {
  expect(agent).toMatch(/^<Agent [AB]>$/);
  expect(model).toMatch(/^<provider-[ab]>\/<model-[ab]>$/);
  journey.modelAliases.set(model, model);
});

Given("no template exists", async ({ journey }) => {
  journey.expectNoTemplates = true;
});

Given("I am signed in as a user who is also an administrator", async ({ page }) => {
  await page.goto("/");
  await assertSignedIn(page);
  await expect(page.getByRole("group", { name: "Workspace view" })).toBeVisible();
});

When("I choose {string} in {string}", async ({ journey, page }, choice, field) => {
  if (field === "Workspace view") {
    const workspace = choice === "Admin workspace" ? "admin" : "user";
    await page.getByRole("group", { name: "Workspace view" }).getByRole("button", { name: workspace, exact: true }).click();
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
  for (const row of dataTable.raw()) {
    for (const cell of row) {
      if (cell.trim()) await expect(scope.getByText(resolveText(journey, cell.trim()), { exact: true })).toBeVisible();
    }
  }
});

When("I open {string}", async ({ journey, page }, destination) => {
  const name = resolveText(journey, destination);
  await page.getByRole("navigation", { name: "Primary navigation" }).getByRole("link", { name, exact: true }).click();
  if (name === "Templates" && journey.expectNoTemplates) {
    await waitForTemplateList(page);
    const initialCount = await prefixedTemplateRows(page).count();
    expect(initialCount, `no template with run prefix ${runPrefix} should exist`).toBe(0);
    journey.initialCounts.set("Templates", initialCount);
  }
});

When("I press {string}", async ({ journey, page }, control) => {
  const resolved = resolveText(journey, control);
  await clickControl(page, resolved);
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
  await expect(page.getByText(resolveText(journey, text), { exact: true }).first()).toBeVisible();
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
  await scope.getByRole("link").filter({ hasText: resolveText(journey, item) }).click();
});

Then("the field {string} shows {string}", async ({ journey, page }, field, value) => {
  await expect(fieldLocator(page, field)).toHaveValue(resolveText(journey, value));
});

Then("the drop-down {string} offers {string}", async ({ journey, page }, field, offered) => {
  const expected = resolveText(journey, offered);
  const options = await page.getByRole("combobox", { name: field, exact: true }).locator("option").allTextContents();
  expect(options.some((option) => option.includes(expected)), `${field} should offer text containing ${expected}`).toBe(true);
});

When("I type a value the installation does not accept into {string}", async ({ page }, field) => {
  await fieldLocator(page, field).fill("not valid");
});

Then("the page {string} lists {int} new entries", async ({ journey, page }, area, expected) => {
  await page.getByRole("navigation", { name: "Primary navigation" }).getByRole("link", { name: area, exact: true }).click();
  await waitForTemplateList(page);
  const initial = journey.initialCounts.get(area);
  expect(initial, `${area} must record its starting count before comparing new entries`).toBeDefined();
  await expect.poll(() => prefixedTemplateRows(page).count(), {
    message: `${area} should list ${expected} new run-prefixed entries`,
  }).toBe(initial + expected);
});

Then("the message names {string}", async ({ page }, field) => {
  const label = page.getByText(field, { exact: true });
  await expect(label).toBeVisible();
  await expect(label.locator("..").getByRole("alert")).toBeVisible();
});
