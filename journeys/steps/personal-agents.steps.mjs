import { expect } from "@playwright/test";

import { Then } from "./fixtures.mjs";

Then("the agent replies with {string} within {int} min", async ({ page }, expected, minutes) => {
  const conversation = page.locator('[aria-label="Conversation"]');
  await expect(conversation.getByText(expected, { exact: true }).first()).toBeVisible({
    timeout: minutes * 60_000,
  });
  await expect(conversation.locator("[data-tone]").filter({ hasText: /^Done$/ }).first()).toBeVisible();
  await expect(conversation.getByRole("link", { name: "Open Run", exact: true }).first()).toBeVisible();
});
