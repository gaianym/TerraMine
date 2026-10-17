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
import { PNG } from "pngjs";
import puppeteer, { type Browser, type Page } from "puppeteer-core";
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

type CbInfo = { state: string; bg: string; border: string; afterContent: string; afterBorder: string; afterBg: string; rowBg: string; rowBefore: string };
type Tm = {
  selectIds(ids: string[]): Promise<void>;
  selectAll(): Promise<void>;
  probeRows(): Probe[];
  probeCheckboxes(): Record<string, CbInfo>;
};

let server: Server;
let browser: Browser;
let page: Page;
let probe: Probe[] = [];
/** Checkbox states: rows 0,1 selected (header indeterminate) and all selected (header checked). */
let partial: Record<string, CbInfo> = {};
let full: Record<string, CbInfo> = {};
/** Decoded pixels of each checkbox (device pixels) per stage. */
const shots: Record<string, PNG> = {};

async function shoot(tag: string, key: string) {
  const handle = await page.$(`[data-tm-cb="${tag}"]`);
  if (!handle) throw new Error(`no checkbox ${tag}`);
  const buf = await handle.screenshot({ type: "png" });
  shots[key] = PNG.sync.read(Buffer.from(buf));
}

/** Min/max relative luminance over the inner area of a checkbox screenshot. */
function pixelLumRange(png: PNG): { min: number; max: number; darkFraction: number } {
  let min = 1, max = 0, dark = 0, n = 0;
  const pad = Math.round(png.width * 0.2);
  for (let y = pad; y < png.height - pad; y++) {
    for (let x = pad; x < png.width - pad; x++) {
      const i = (y * png.width + x) * 4;
      const l = lum([png.data[i], png.data[i + 1], png.data[i + 2], 1]);
      min = Math.min(min, l);
      max = Math.max(max, l);
      if (l < 0.05) dark++;
      n++;
    }
  }
  return { min, max, darkFraction: dark / n };
}

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
    page = await browser.newPage();
    await page.setViewport({ width: 1200, height: 400, deviceScaleFactor: 2 });
    await page.goto(`http://127.0.0.1:${port}/`);
    await page.waitForFunction(() => document.title === "READY", { timeout: 20_000 });
    const tm = () => (window as unknown as { __tm: Tm }).__tm;
    await page.evaluate(async () => (window as unknown as { __tm: Tm }).__tm.selectIds(["0", "1"]));
    partial = (await page.evaluate(() => (window as unknown as { __tm: Tm }).__tm.probeCheckboxes())) as Record<string, CbInfo>;
    await shoot("header", "header-indeterminate");
    await shoot("row-0", "row-checked");
    await shoot("row-2", "row-unchecked");
    await page.evaluate(async () => (window as unknown as { __tm: Tm }).__tm.selectAll());
    full = (await page.evaluate(() => (window as unknown as { __tm: Tm }).__tm.probeCheckboxes())) as Record<string, CbInfo>;
    await shoot("header", "header-checked");
    probe = (await page.evaluate(() => (window as unknown as { __tm: Tm }).__tm.probeRows())) as Probe[];
    void tm;
  }, 90_000);

  afterAll(async () => {
    await browser?.close();
    server?.close();
  });

  it("renders every row selected", () => {
    expect(probe.length).toBeGreaterThan(0);
    expect(probe.every((r) => r.selected)).toBe(true);
  });

  it("selected-row background is opaque, neutral grey and dark (not legacy #bbb, not tinted)", () => {
    for (const r of probe) {
      const before = parse(r.beforeBg);
      expect(before[3], `selected overlay ${r.beforeBg} must be opaque`).toBe(1);
      const [R, G, B] = before;
      expect(Math.max(R, G, B) - Math.min(R, G, B), `${r.beforeBg} must be neutral grey`).toBeLessThanOrEqual(3);
      expect(lum(before), `${r.beforeBg} must be dark`).toBeLessThan(0.05);
      // Visibly different from an unselected row.
      expect(contrast(before, parse(r.rowBg))).toBeGreaterThan(1.05);
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

  describe("selection checkboxes", () => {
    it("checked, unchecked and indeterminate states are styled differently", () => {
      expect(partial["row-0"].state).toBe("checked");
      expect(partial["row-2"].state).toBe("unchecked");
      expect(partial.header.state).toBe("indeterminate");
      expect(full.header.state).toBe("checked");
      expect(partial["row-0"].bg).not.toBe(partial["row-2"].bg);
      expect(partial["row-0"].afterContent).not.toBe("none");
      expect(partial["row-2"].afterContent).toBe("none");
      expect(partial.header.afterContent).not.toBe("none");
    });

    it("checked fill stands out (>= 3:1) on the selected grey row; unchecked border on dark rows", () => {
      const checked = partial["row-0"];
      const selBg = over(parse(checked.rowBefore), parse(checked.rowBg));
      expect(contrast(parse(checked.bg), selBg)).toBeGreaterThanOrEqual(3);
      const un = partial["row-2"];
      expect(contrast(parse(un.border), parse(un.rowBg))).toBeGreaterThanOrEqual(3);
    });

    it("checkmark and indeterminate bar have >= 3:1 against the fill", () => {
      const c = partial["row-0"];
      expect(contrast(parse(c.afterBorder), parse(c.bg))).toBeGreaterThanOrEqual(3);
      const h = partial.header;
      expect(contrast(parse(h.afterBg), parse(h.bg))).toBeGreaterThanOrEqual(3);
    });

    it("renders differently in pixels: filled box with a visible mark vs hollow box", () => {
      const checked = pixelLumRange(shots["row-checked"]);
      const unchecked = pixelLumRange(shots["row-unchecked"]);
      const indet = pixelLumRange(shots["header-indeterminate"]);
      const hdr = pixelLumRange(shots["header-checked"]);
      // Checked: bright fill + dark tick inside, i.e. high internal contrast.
      for (const r of [checked, indet, hdr]) {
        expect((r.max + 0.05) / (r.min + 0.05)).toBeGreaterThanOrEqual(3);
        expect(r.max).toBeGreaterThan(0.5);
      }
      // Unchecked: interior stays dark (no fill).
      expect(unchecked.max).toBeLessThan(0.2);
      expect(Buffer.compare(shots["row-checked"].data, shots["row-unchecked"].data)).not.toBe(0);
      expect(Buffer.compare(shots["header-checked"].data, shots["header-indeterminate"].data)).not.toBe(0);
    });
  });
});
