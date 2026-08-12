// Puppeteer connectOverCDP smoke (non-pixel subset).
// npm i puppeteer-core && node docs/smokes/puppeteer-cdp.mjs
import puppeteer from "puppeteer-core";

const url = process.env.HYPERION_CDP || "http://127.0.0.1:9222";
const browser = await puppeteer.connect({
  browserURL: url,
  headers: process.env.HYPERION_AUTH_TOKEN
    ? { Authorization: `Bearer ${process.env.HYPERION_AUTH_TOKEN}` }
    : {},
});
const page = await browser.newPage();
await page.goto(process.env.SMOKE_URL || "https://example.com", { waitUntil: "domcontentloaded" });
const title = await page.evaluate(() => document.title);
if (!title) throw new Error("empty title");
await page.evaluate(() => { document.body.setAttribute("data-hyperion", "1"); });
console.log("puppeteer smoke ok", title);
await browser.disconnect();
