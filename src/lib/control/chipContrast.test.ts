import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { CHIP_CLASS } from "./statusChip";

// Demo bug: on selected rows the status badge text was hard to read.
// Fix (single change): darker selected-row tint. This test pins WCAG AA
// (>= 4.5:1) for all row text and every badge colour on selected rows,
// including hover+selected and odd rows, and even if the tint overlay were
// painted on top of the text.
const css = readFileSync(resolve(__dirname, "../../app.css"), "utf8");

type RGBA = [number, number, number, number];

function parseColor(raw: string): RGBA {
  const v = raw.trim();
  const hex = v.match(/^#([0-9a-f]{6})$/i);
  if (hex) {
    const n = Number.parseInt(hex[1], 16);
    return [(n >> 16) & 255, (n >> 8) & 255, n & 255, 1];
  }
  const fn = v.match(/^rgba?\(([^)]*)\)$/);
  if (!fn) throw new Error(`unparsed colour ${v}`);
  const p = fn[1].split(/[\s,/]+/).filter(Boolean).map(Number);
  return [p[0], p[1], p[2], p.length > 3 ? p[3] : 1];
}

function cssVar(name: string): RGBA {
  const m = css.match(new RegExp(`${name}\\s*:\\s*([^;]+);`));
  if (!m) throw new Error(`missing ${name}`);
  const val = m[1].trim();
  const ref = val.match(/^var\((--[\w-]+)\)$/);
  return ref ? cssVar(ref[1]) : parseColor(val);
}

function chipColor(cls: string): RGBA {
  const m = css.match(new RegExp(`\\.${cls}\\s*\\{[^}]*?(?:^|[;{\\s])color:\\s*(rgba?\\([^)]*\\)|#[0-9a-f]{6})`, "i"));
  if (!m) throw new Error(`missing colour for .${cls}`);
  return parseColor(m[1]);
}

/** Source-over composite of a (possibly translucent) colour onto an opaque one. */
function over(top: RGBA, base: RGBA): RGBA {
  const a = top[3];
  return [0, 1, 2].map((i) => top[i] * a + base[i] * (1 - a)).concat(1) as RGBA;
}

function lum(c: RGBA): number {
  const f = (x: number) => {
    const s = x / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * f(c[0]) + 0.7152 * f(c[1]) + 0.0722 * f(c[2]);
}

function contrast(a: RGBA, b: RGBA): number {
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

const selected = cssVar("--ag-selected-row-background-color");
const rowBases: Record<string, RGBA> = {
  even: cssVar("--tm-bg"),
  odd: cssVar("--ag-odd-row-background-color"),
  hover: cssVar("--ag-row-hover-color"),
};
const texts: Record<string, RGBA> = {
  "row text": cssVar("--tm-text"),
  "muted row text": cssVar("--tm-text-muted"),
  ...Object.fromEntries(Object.values(CHIP_CLASS).map((c) => [`.${c}`, chipColor(c)])),
};

describe("selected-row legibility (WCAG AA >= 4.5:1)", () => {
  for (const [baseName, base] of Object.entries(rowBases)) {
    const bg = over(selected, base);
    for (const [textName, fg] of Object.entries(texts)) {
      it(`${textName} on selected ${baseName} row`, () => {
        expect(contrast(fg, bg)).toBeGreaterThanOrEqual(4.5);
        // Worst case: tint overlay painted above the text as well.
        expect(contrast(over(selected, fg), bg)).toBeGreaterThanOrEqual(4.5);
      });
    }
  }

  it("selected tint is darker than the demo value (alpha <= 0.1)", () => {
    expect(selected[3]).toBeLessThanOrEqual(0.1);
  });
});
