// Playwright connectOverCDP smoke.
import { chromium } from "playwright";

const endpoint = process.env.HYPERION_CDP || "http://127.0.0.1:9222";
const browser = await chromium.connectOverCDP(endpoint);
const context = browser.contexts()[0] || (await browser.newContext());
const page = await context.newPage();
await page.goto(process.env.SMOKE_URL || "https://example.com");
const title = await page.title();
if (!title) throw new Error("empty title");
console.log("playwright smoke ok", title);
await browser.close();
