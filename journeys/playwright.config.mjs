import { resolve } from "node:path";

import { defineConfig } from "@playwright/test";
import { defineBddConfig } from "playwright-bdd";

const execution = process.env.STEWARD_JOURNEY_EXECUTION ?? "validation";
const tags = process.env.STEWARD_JOURNEY_TAG_EXPRESSION ?? "@smoke";

const testDir = defineBddConfig({
  features: [
    "J01-administrator-sets-up-a-new-installation.feature",
    "J02-new-user-gets-going.feature",
    "J03-user-needs-more-than-the-template-allows.feature",
    "J04-user-runs-a-task-on-a-repository.feature",
    "J05-user-runs-a-task-with-scratch-space.feature",
    "J06-user-asks-for-more-than-the-envelope-allows.feature",
    "J07-user-watches-and-stops-a-run.feature",
    "J08-user-chooses-agent-and-model.feature",
    "J09-administrator-oversight.feature",
    "J10-operator-changes-configuration.feature",
    "J11-user-runs-a-task-with-jira.feature",
  ],
  steps: "steps/*.mjs",
  outputDir: `.generated/${execution}`,
  tags,
  missingSteps: "fail-on-gen",
});

export default defineConfig({
  testDir,
  outputDir: `../target/journeys/results/${execution}`,
  forbidOnly: true,
  fullyParallel: false,
  workers: 1,
  timeout: 360_000,
  expect: { timeout: 10_000 },
  reporter: execution === "known-failing"
    ? [["list"], ["json", { outputFile: resolve("target/journeys/known-failing-report.json") }]]
    : "list",
  use: {
    baseURL: process.env.STEWARD_JOURNEY_BASE_URL,
    actionTimeout: 15_000,
    browserName: "chromium",
    headless: true,
    ignoreHTTPSErrors: process.env.STEWARD_JOURNEY_IGNORE_HTTPS_ERRORS === "true",
    navigationTimeout: 30_000,
    screenshot: "only-on-failure",
    storageState: process.env.STEWARD_JOURNEY_STORAGE_STATE,
    trace: "retain-on-failure",
    video: "off",
  },
});
