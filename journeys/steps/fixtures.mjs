import { expect } from "@playwright/test";
import { createBdd, test as base } from "playwright-bdd";

const shared = {
  checkedPrefixes: new Set(),
};

export const test = base.extend({
  journey: async ({ page }, use, testInfo) => {
    const consoleErrors = [];
    const pageErrors = [];
    const onConsole = (message) => {
      if (message.type() === "error") consoleErrors.push(message.text());
    };
    const onPageError = (error) => pageErrors.push(error.message);
    page.on("console", onConsole);
    page.on("pageerror", onPageError);
    await use({
      expectNoTemplates: false,
      extended: testInfo.tags.includes("@extended"),
      currentTemplate: null,
      modelAliases: new Map(),
      initialCounts: new Map(),
      integrationStates: new Map(),
      knownIssue: testInfo.tags.find((tag) => tag.startsWith("@known-failing-")) ?? null,
      requests: new Map(),
      repository: null,
      run: null,
      initialRunIds: null,
      runRuntimeMinutes: null,
      selectedModels: [],
      shared,
      transientSeen: new Set(),
    });
    page.off("console", onConsole);
    page.off("pageerror", onPageError);
    expect(consoleErrors, "the journey must not emit browser console errors").toEqual([]);
    expect(pageErrors, "the journey must not emit uncaught page errors").toEqual([]);
  },
});

export const { Given, When, Then } = createBdd(test);
