// Renders an SVG to a square PNG with a transparent background, through
// Playwright's Chromium: node scripts/render-svg.js <in.svg> <out.png> <size>
const { chromium } = require('playwright');
const fs = require('fs');

(async () => {
  const [svgPath, out, sizeArg] = process.argv.slice(2);
  const size = parseInt(sizeArg || '1024', 10);
  const svg = fs.readFileSync(svgPath).toString('base64');
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: size, height: size }, deviceScaleFactor: 1 });
  await page.setContent(
    `<html><body style="margin:0;background:transparent">` +
      `<img id="i" src="data:image/svg+xml;base64,${svg}" width="${size}" height="${size}"></body></html>`,
  );
  await page.waitForFunction(() => document.getElementById('i').complete);
  await page.screenshot({ path: out, omitBackground: true });
  await browser.close();
})();
