import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  testMatch: "**/*.spec.mjs",
  forbidOnly: true,
  fullyParallel: false,
  workers: 1,
  // A clean CI runner compiles two Rust loopback demos before the first journey.
  timeout: 120_000,
  reporter: "list",
  use: {
    browserName: "chromium",
    headless: true,
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
    video: "off",
  },
});
