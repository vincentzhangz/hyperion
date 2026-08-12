import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  use: { baseURL: process.env.HYPERION_DASHBOARD_URL || "http://127.0.0.1:9222" },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
});
