import {
  ClientSideRowModelModule,
  ColumnApiModule,
  RowSelectionModule,
  type GridOptions,
} from "ag-grid-community";

/** AG Grid modules the results grid registers (shared with the browser theme test fixture). */
export const RESULTS_GRID_MODULES = [ClientSideRowModelModule, ColumnApiModule, RowSelectionModule];

/** Classes on the element the results grid is created in (see +page.svelte). */
export const RESULTS_GRID_HOST_CLASS = "ag-theme-quartz-dark h-full w-full min-h-0 text-xs";

/**
 * Selection + theme-relevant grid options for the results grid. No `theme`
 * option, so createGrid uses the Theming API wrapper; the browser test
 * renders with exactly these options against the real app.css.
 */
export const RESULTS_GRID_SELECTION_OPTIONS = {
  rowSelection: {
    mode: "multiRow",
    checkboxes: true,
    headerCheckbox: true,
    enableClickSelection: true,
    enableSelectionWithoutKeys: false,
  },
  selectionColumnDef: {
    pinned: "left",
    lockPinned: true,
    width: 48,
    maxWidth: 52,
    minWidth: 44,
    resizable: false,
    suppressHeaderMenuButton: true,
    headerName: "",
  },
} satisfies Partial<GridOptions>;
