# Terra Mine Theming Guide

This document defines the visual theme used by the app and AG Grid.

## Source Of Truth

- Global stylesheet: `src/app.css`
- App palette variables are defined in `:root` with `--tm-*` tokens.
- AG Grid (`.ag-theme-quartz-dark`) is overridden to use the same tokens.

## Theme Tokens

The following variables should be treated as the canonical palette:

- `--tm-bg`: primary app background
- `--tm-surface`: elevated surface background
- `--tm-surface-2`: secondary elevated surface
- `--tm-border`: default border color
- `--tm-text`: primary foreground text
- `--tm-text-muted`: muted labels and helper text
- `--tm-accent`: interactive accent color (emerald)

## Styling Rules

- Use `--tm-*` tokens for new custom CSS instead of hardcoded hex values.
- Prefer Tailwind zinc/emerald utility classes only when they map to the same palette intent.
- Keep AG Grid color variables mapped to `--tm-*` to avoid visual drift.
- Keep AG Grid font sizing and family consistent with app typography.

## AG Grid Consistency

AG Grid should remain on `ag-theme-quartz-dark`, with overrides in `src/app.css` for:

- Backgrounds (grid, headers, rows, hover)
- Borders/separators
- Input/focus states
- Selection and accent states
- Typography

### Required AG Grid Overrides

To avoid AG Grid falling back to light backgrounds in embedded runtimes, keep these override groups in `src/app.css`:

- **Theme variables**
  - `--ag-background-color`
  - `--ag-data-background-color`
  - `--ag-foreground-color`
  - `--ag-data-color`
  - `--ag-header-background-color`
  - `--ag-header-foreground-color`
- **Container layers**
  - `.ag-root-wrapper`, `.ag-root-wrapper-body`
  - `.ag-header`, `.ag-header-viewport`
  - `.ag-body-viewport`, `.ag-center-cols-viewport`
  - `.ag-center-cols-container`, pinned column containers
- **Row and cell layers**
  - `.ag-row`, `.ag-row-odd`, `.ag-row-hover`
  - `.ag-cell`

If table rows appear too bright, verify these container and row/cell overrides first before changing palette tokens.

## When Updating Theme

When changing the app look:

1. Update `--tm-*` values in `src/app.css`.
2. Verify AG Grid still matches (header, rows, borders, hover, selection).
3. Check key UI surfaces (header, settings panel, results panel) for contrast consistency.
4. Run `npm run build` to ensure CSS changes compile cleanly.
