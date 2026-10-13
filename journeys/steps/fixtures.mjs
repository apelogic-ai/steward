import { expect } from "@playwright/test";
import { createBdd, test as base } from "playwright-bdd";

export const test = base.extend({
  journey: async ({ page }, use) => {
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
      modelAliases: new Map(),
      initialCounts: new Map(),
      selectedModels: [],
    });
    page.off("console", onConsole);
    page.off("pageerror", onPageError);
    expect(consoleErrors, "the journey must not emit browser console errors").toEqual([]);
    expect(pageErrors, "the journey must not emit uncaught page errors").toEqual([]);
  },
});

export const { Given, When, Then } = createBdd(test);
