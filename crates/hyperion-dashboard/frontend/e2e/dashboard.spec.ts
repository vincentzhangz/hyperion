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
    const sandbox = await iframe.getAttribute("sandbox");
    expect(sandbox).toBe("allow-same-origin");
    expect(sandbox).not.toContain("allow-scripts");
    expect(sandbox).not.toContain("allow-forms");
    expect(sandbox).not.toContain("allow-top-navigation");
  }
});

test("local profile opens an interactive DOM-mirror workspace", async ({ page }) => {
  test.skip(!process.env.HYPERION_E2E, "requires running Hyperion");
  const token = process.env.HYPERION_AUTH_TOKEN || "e2etoken";
  await page.goto("/ui/");
  if (await page.getByLabel("Bearer token").count()) {
    await page.getByLabel("Bearer token").fill(token);
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page.getByRole("heading", { name: "Cluster overview" })).toBeVisible();
  }

  await page.getByRole("link", { name: "Profiles" }).click();
  await page.getByLabel("Search profiles").fill("e2e-browser");
  await page.getByRole("button", { name: "Create" }).click();
  await expect(page.getByRole("button", { name: "Open browser" }).first()).toBeEnabled();
  await page.getByRole("button", { name: "Open browser" }).first().click();
  await expect(page.getByTestId("profile-browser")).toBeVisible();
  await expect(page.getByText(/interactive DOM mirror/i)).toBeVisible();

  await page.getByTestId("preview-url").fill("about:blank");
  await page.getByTestId("preview-go").click();
  const iframe = page.locator("iframe.preview-frame");
  await expect(iframe).toBeVisible({ timeout: 15_000 });
  await expect(iframe).toHaveAttribute("sandbox", "allow-same-origin");

  await page.getByTestId("preview-url").fill("https://example.com/");
  await page.getByTestId("preview-go").click();
  await expect.poll(async () => iframe.evaluate((el: HTMLIFrameElement) => el.srcdoc), { timeout: 30_000 }).toMatch(
    /Example Domain|example/i,
  );

  await iframe.evaluate((el: HTMLIFrameElement) => {
    const doc = el.contentDocument;
    const link = doc?.querySelector("a");
    link?.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
  });

  const hostile = `<a href="/ui/" target="_top">leave</a><form action="/ui/"><button type="submit">go</button></form><script>window.top.hacked=1</script>`;
  await iframe.evaluate((el: HTMLIFrameElement, html: string) => {
    el.srcdoc = html;
  }, hostile);
  await expect.poll(async () => page.evaluate(() => (window as Window & { hacked?: number }).hacked)).toBeUndefined();
  await expect(page).toHaveURL(/\/ui\//);
});
