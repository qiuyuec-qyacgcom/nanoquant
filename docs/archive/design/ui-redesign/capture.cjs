const fs = require("fs");
const path = require("path");
const { pathToFileURL } = require("url");
const { createRequire } = require("module");

const requireFromFrontend = createRequire("F:/cb/nanoquant/frontend/package.json");
const { chromium } = requireFromFrontend("playwright");

const dir = __dirname;
const outDir = path.join(dir, "screenshots");
fs.mkdirSync(outDir, { recursive: true });

const pages = [
  { file: "public-home.html", out: "01-public-home.png", width: 1440, height: 900, fullPage: true },
  { file: "login.html", out: "02-login.png", width: 1440, height: 900, fullPage: true },
  { file: "admin-dashboard.html", out: "03-admin-dashboard.png", width: 1440, height: 900, fullPage: true },
  { file: "admin-settings.html", out: "04-admin-settings.png", width: 1440, height: 900, fullPage: true },
  { file: "admin-risk.html", out: "05-admin-risk.png", width: 1440, height: 900, fullPage: true },
  { file: "mobile-dashboard.html", out: "06-mobile-dashboard.png", width: 390, height: 844, fullPage: false },
];

(async () => {
  const browser = await chromium.launch({ headless: true });
  const results = [];
  for (const item of pages) {
    const page = await browser.newPage({
      viewport: { width: item.width, height: item.height },
      deviceScaleFactor: 1,
    });
    const url = pathToFileURL(path.join(dir, item.file)).href;
    await page.goto(url, { waitUntil: "load" });
    await page.evaluate(() => document.fonts.ready);
    await page.waitForTimeout(250);
    const metrics = await page.evaluate(() => ({
      width: document.documentElement.scrollWidth,
      height: document.documentElement.scrollHeight,
      viewportWidth: window.innerWidth,
      viewportHeight: window.innerHeight,
    }));
    await page.screenshot({ path: path.join(outDir, item.out), fullPage: item.fullPage });
    results.push({ ...item, metrics });
    await page.close();
  }
  await browser.close();
  console.log(JSON.stringify(results, null, 2));
})().catch((error) => {
  console.error(error);
  process.exit(1);
});