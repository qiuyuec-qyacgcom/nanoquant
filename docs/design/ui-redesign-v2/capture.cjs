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
  { file: "concept.html", query: "?variant=a", out: "01-phosphor-terminal.png" },
  { file: "concept.html", query: "?variant=b", out: "02-graphite-blueprint.png" },
  { file: "concept.html", query: "?variant=c", out: "03-obsidian-acid.png" },
];

(async () => {
  const browser = await chromium.launch({ headless: true });
  const results = [];
  for (const item of pages) {
    const page = await browser.newPage({
      viewport: { width: 1440, height: 900 },
      deviceScaleFactor: 1,
    });
    const url = pathToFileURL(path.join(dir, item.file)).href + item.query;
    await page.goto(url, { waitUntil: "load" });
    await page.evaluate(() => document.fonts.ready);
    await page.waitForTimeout(250);
    const metrics = await page.evaluate(() => ({
      width: document.documentElement.scrollWidth,
      height: document.documentElement.scrollHeight,
    }));
    await page.screenshot({ path: path.join(outDir, item.out), fullPage: true });
    results.push({ ...item, metrics });
    await page.close();
  }
  await browser.close();
  console.log(JSON.stringify(results, null, 2));
})().catch((error) => {
  console.error(error);
  process.exit(1);
});