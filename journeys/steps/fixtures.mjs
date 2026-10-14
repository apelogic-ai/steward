import { expect } from "@playwright/test";
import { createBdd, test as base } from "playwright-bdd";

import {
  onlyOnboardingRequestUnavailable,
  onlyZeroBudgetTaskSaveRejected,
} from "../onboarding-console-errors.mjs";

const shared = {
  checkedPrefixes: new Set(),
};

export const test = base.extend({
  journey: async ({ page }, use, testInfo) => {
    const consoleErrors = [];
    const pageErrors = [];
    const taskSaveRejections = [];
    const onConsole = (message) => {
      if (message.type() === "error") {
        consoleErrors.push({ text: message.text(), url: message.location().url });
      }
    };
    const onPageError = (error) => pageErrors.push(error.message);
    const onResponse = (response) => {
      try {
        const source = new URL(response.url());
        if (response.status() === 422 && source.pathname === "/app/api/v1/tasks" && source.search === "") {
          taskSaveRejections.push({
            method: response.request().method(),
            status: response.status(),
            url: response.url(),
          });
        }
      } catch {
        // A malformed response URL cannot match the expected local Task endpoint.
      }
    };
    page.on("console", onConsole);
    page.on("pageerror", onPageError);
    page.on("response", onResponse);
    const state = {
      expectNoTemplates: false,
      extended: testInfo.tags.includes("@extended"),
      jira: testInfo.tags.includes("@needs-jira"),
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
      statedSingleRunLimit: null,
      selectedModels: [],
      shared,
      transientSeen: new Set(),
    };
    await use(state);
    page.off("console", onConsole);
    page.off("pageerror", onPageError);
    page.off("response", onResponse);
    expect(pageErrors, "the journey must not emit uncaught page errors").toEqual([]);
    if (state.knownIssue === "@known-failing-478" && onlyOnboardingRequestUnavailable(consoleErrors, page.url())) {
      throw new Error(`[steward-known-failure:#478] the onboarding page emitted browser console errors: ${consoleErrors.length}`);
    }
    if (state.statedSingleRunLimit === "0" && onlyZeroBudgetTaskSaveRejected(consoleErrors, taskSaveRejections, page.url())) return;
    expect(consoleErrors, "the journey must not emit browser console errors").toEqual([]);
  },
});

export const { Given, When, Then } = createBdd(test);
