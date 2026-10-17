/**
 * Demo bug 3 regression test: render the results grid in a real browser with
 * the app's real CSS cascade (Tailwind + AG Grid legacy CSS + the Theming API
 * styles createGrid injects + app.css) and assert the *effective* selected-row
 * background gives every in-row text colour >= 4.5:1.
 *
 * The previous static check only read our CSS token; the Theming API wrapper
 * picked up legacy ag-grid.css's #bbb instead and rows went light grey.
 */
import { createServer, type Server } from "node:http";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { extname, join, resolve } from "node:path";
import tailwindcss from "@tailwindcss/vite";
import puppeteer, { type Browser } from "puppeteer-core";
import { build } from "vite";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

type RGBA = [number, number, number, number];
type Probe = {
  selected: boolean;
  rowBg: string;
  beforeBg: string;
  beforeImage: string;
  texts: { cls: string; text: string; color: string }[];
};

function chromePath(): string | null {
  const c = [
    process.env.CHROME_PATH,
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
  ].filter(Boolean) as string[];
  return c.find((p) => existsSync(p)) ?? null;
}

const CHROME = chromePath();
if (!CHROME && process.env.CI) throw new Error("Chrome not found on CI runner; set CHROME_PATH");

function parse(c: string): RGBA {
  const m = c.match(/rgba?\(([^)]+)\)/);
  if (!m) throw new Error(`unparsed colour ${c}`);
  const p = m[1].split(/[\s,/]+/).filter(Boolean).map(Number);
  return [p[0], p[1], p[2], p.length > 3 ? p[3] : 1];
}
const over = (t: RGBA, b: RGBA): RGBA => [0, 1, 2].map((i) => t[i] * t[3] + b[i] * (1 - t[3])).concat(1) as RGBA;
const lum = (c: RGBA) => {
  const f = (x: number) => ((x /= 255) <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4);
  return 0.2126 * f(c[0]) + 0.7152 * f(c[1]) + 0.0722 * f(c[2]);
};
const contrast = (a: RGBA, b: RGBA) => {
  const [h, l] = [lum(a), lum(b)].sort((x, y) => y - x);
  return (h + 0.05) / (l + 0.05);
};

/**
 * The up-ticker uses the brand accent (#47823e), which is 4.28:1 even on an
 * unselected row, so it can only meet AA by changing the text colour (out of
 * scope for this fix). Held to the 3:1 non-text/large-text floor so it can't
 * regress further; everything else must meet 4.5:1.
 */
const AA_EXCEPTIONS: Record<string, number> = { "tm-move-up": 3 };

let server: Server;
let browser: Browser;
let probe: Probe[] = [];

describe.skipIf(!CHROME)("selected rows in a real browser", () => {
  beforeAll(async () => {
    const root = resolve(__dirname, "../../../tests/fixtures/grid-theme");
    const outDir = mkdtempSync(join(tmpdir(), "tm-grid-theme-"));
    await build({
      root,
      configFile: false,
      logLevel: "error",
      plugins: [tailwindcss()],
      build: { outDir, emptyOutDir: true },
    });
    const types: Record<string, string> = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml" };
    server = createServer((req, res) => {
      const path = join(outDir, req.url === "/" ? "index.html" : decodeURIComponent(req.url!.split("?")[0]));
      let body: Buffer;
      try {
        body = readFileSync(path);
      } catch {
        res.writeHead(404).end();
        return;
      }
      res.writeHead(200, { "content-type": types[extname(path)] ?? "application/octet-stream" });
      res.end(body);
    }).listen(0);
    await new Promise((r) => server.once("listening", r));
    const port = (server.address() as { port: number }).port;
    browser = await puppeteer.launch({ executablePath: CHROME!, headless: true, args: ["--no-sandbox"] });
    const page = await browser.newPage();
    await page.setViewport({ width: 1200, height: 400 });
    await page.goto(`http://127.0.0.1:${port}/`);
    await page.waitForFunction(() => document.title === "READY", { timeout: 20_000 });
    probe = (await page.evaluate(() => (window as unknown as { __gridTheme: Probe[] }).__gridTheme)) as Probe[];
  }, 90_000);

  afterAll(async () => {
    await browser?.close();
    server?.close();
  });

  it("renders every row selected", () => {
    expect(probe.length).toBeGreaterThan(0);
    expect(probe.every((r) => r.selected)).toBe(true);
  });

  it("effective selected-row background is dark (not the legacy #bbb grey)", () => {
    for (const r of probe) {
      const bg = over(parse(r.beforeBg), parse(r.rowBg));
      expect(lum(bg), `effective ${JSON.stringify(bg)} from ${r.beforeBg} over ${r.rowBg}`).toBeLessThan(0.05);
    }
  });

  it("every in-row text colour has >= 4.5:1 against the effective selected background", () => {
    const failures: string[] = [];
    let checked = 0;
    for (const r of probe) {
      const bg = over(parse(r.beforeBg), parse(r.rowBg));
      for (const t of r.texts) {
        const ratio = contrast(over(parse(t.color), bg), bg);
        const key = Object.keys(AA_EXCEPTIONS).find((k) => t.cls.includes(k));
        const min = key ? AA_EXCEPTIONS[key] : 4.5;
        checked++;
        if (ratio < min) failures.push(`${t.cls} "${t.text}" ${t.color} on ${JSON.stringify(bg)} = ${ratio.toFixed(2)} < ${min}`);
      }
    }
    expect(checked).toBeGreaterThanOrEqual(probe.length * 15);
    expect(failures).toEqual([]);
  });
});
