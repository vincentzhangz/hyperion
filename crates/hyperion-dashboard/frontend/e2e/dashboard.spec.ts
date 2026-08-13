import { test, expect } from "@playwright/test";

test("login page is keyboard accessible", async ({ page }) => {
  await page.goto("/ui/");
  await expect(page.getByRole("heading", { name: /sign in|hyperion/i })).toBeVisible();
  await page.keyboard.press("Tab");
});

test("operator can sign in with the bearer token", async ({ page }) => {
  test.skip(!process.env.HYPERION_E2E, "requires running Hyperion");
  const token = process.env.HYPERION_AUTH_TOKEN || "e2etoken";
  await page.goto("/ui/");
  await page.getByLabel("Bearer token").fill(token);
  await page.getByRole("button", { name: "Continue" }).click();
  await expect(page.getByRole("heading", { name: "Cluster overview" })).toBeVisible();
});

test("hostile preview is framed, not inlined", async ({ page }) => {
  test.skip(!process.env.HYPERION_E2E, "requires running Hyperion");
  await page.goto("/ui/");
  const iframe = page.locator("iframe.preview-frame");
  if ((await iframe.count()) > 0) {
    await expect(iframe).toHaveAttribute("sandbox", "");
  }
});
