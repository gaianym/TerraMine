// Renders the results grid with the app's real CSS (Tailwind + ag-grid.css +
// ag-theme-quartz.css + app.css) and the app's real selection options, selects
// every row, and exposes what the browser actually paints for selected rows.
import "../../../src/app.css";
import { createGrid, ModuleRegistry } from "ag-grid-community";
import {
  RESULTS_GRID_HOST_CLASS,
  RESULTS_GRID_MODULES,
  RESULTS_GRID_SELECTION_OPTIONS,
} from "../../../src/lib/grid/gridSelectionOptions";

ModuleRegistry.registerModules(RESULTS_GRID_MODULES);

const host = document.getElementById("g")!;
host.className = RESULTS_GRID_HOST_CLASS;

function el(tag: string, cls: string, text: string) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  e.textContent = text;
  return e;
}

// One sample per in-row text style used by the results grid.
const SAMPLES: Array<[string, string, string]> = [
  ["span", "", "192.0.2.10"],
  ["span", "tm-fan-rpm-num", "5860"],
  ["span", "tm-fan-rpm-unit", " rpm"],
  ["button", "tm-control-btn", "Reboot"],
  ["span", "tm-chip tm-chip-ok", "back online"],
  ["span", "tm-chip tm-chip-warn", "rebooting…"],
  ["span", "tm-chip tm-chip-bad", "failed"],
  ["span", "tm-chip tm-chip-busy", "verifying…"],
  ["span", "tm-chip tm-chip-muted", "queued"],
  ["span", "tm-chip tm-chip-dry", "dry run"],
  ["span", "tm-move-indicator tm-move-up", "↑ +0.05 TH/s"],
  ["span", "tm-move-indicator tm-move-down", "↓ -0.05 TH/s"],
  ["span", "tm-move-indicator tm-move-temp-up", "↑ +2 °C"],
  ["span", "tm-move-indicator tm-move-temp-down", "↓ -2 °C"],
];

const rowData = [0, 1, 2, 3].map((i) => ({ id: String(i), ip: `192.0.2.${10 + i}`, mac: "00:00:5E:00:53:0" + i }));

createGrid(host, {
  ...RESULTS_GRID_SELECTION_OPTIONS,
  rowData,
  getRowId: (p) => p.data.id,
  columnDefs: [
    { field: "ip" },
    { field: "mac" },
    {
      colId: "samples",
      width: 900,
      cellRenderer: () => {
        const wrap = document.createElement("span");
        for (const [tag, cls, text] of SAMPLES) wrap.appendChild(el(tag, cls, text));
        return wrap;
      },
    },
  ],
  onFirstDataRendered: (e) => {
    e.api.selectAll();
    // Let the selection classes paint.
    setTimeout(() => {
      const rows = [...document.querySelectorAll<HTMLElement>(".ag-center-cols-container .ag-row")];
      const result = rows.map((row) => {
        const before = getComputedStyle(row, "::before");
        const texts = [...row.querySelectorAll<HTMLElement>(".ag-cell, .ag-cell *")]
          .filter((n) => [...n.childNodes].some((c) => c.nodeType === Node.TEXT_NODE && c.textContent!.trim()))
          .map((n) => ({ cls: n.className || n.tagName.toLowerCase(), text: n.textContent!.trim(), color: getComputedStyle(n).color }));
        return {
          selected: row.classList.contains("ag-row-selected"),
          rowBg: getComputedStyle(row).backgroundColor,
          beforeBg: before.backgroundColor,
          beforeImage: before.backgroundImage,
          texts,
        };
      });
      (window as unknown as { __gridTheme: unknown }).__gridTheme = result;
      document.title = "READY";
    }, 300);
  },
});
