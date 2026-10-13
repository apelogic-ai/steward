import { defineConfig } from "@playwright/test";
import { defineBddConfig } from "playwright-bdd";

const execution = process.env.STEWARD_JOURNEY_EXECUTION ?? "validation";
const tags = process.env.STEWARD_JOURNEY_TAG_EXPRESSION ?? "@smoke";

const testDir = defineBddConfig({
  features: "J01-administrator-sets-up-a-new-installation.feature",
  steps: "steps/*.mjs",
  outputDir: `../target/journeys/generated/${execution}`,
  tags,
  missingSteps: "fail-on-gen",
});

export default defineConfig({
  testDir,
  outputDir: `../target/journeys/results/${execution}`,
  forbidOnly: true,
  fullyParallel: false,
  workers: 1,
  timeout: 120_000,
  reporter: "list",
  use: {
    baseURL: process.env.STEWARD_JOURNEY_BASE_URL,
    browserName: "chromium",
    headless: true,
    screenshot: "only-on-failure",
    storageState: process.env.STEWARD_JOURNEY_STORAGE_STATE,
    trace: "retain-on-failure",
    video: "off",
  },
});
