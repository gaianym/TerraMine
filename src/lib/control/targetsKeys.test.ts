import { describe, expect, it, vi } from "vitest";
import { applyTargetsShortcut, targetsShortcutAction } from "./targetsKeys";

describe("targetsShortcutAction", () => {
  it("handles Ctrl+A as selectAll with preventDefault (WebKit quit path)", () => {
    expect(targetsShortcutAction({ key: "a", ctrlKey: true, metaKey: false, altKey: false })).toEqual({
      type: "selectAll",
      stopPropagation: true,
      preventDefault: true,
    });
    expect(targetsShortcutAction({ key: "A", metaKey: true, ctrlKey: false, altKey: false }).type).toBe(
      "selectAll",
    );
  });

  it("contains Ctrl+V / Ctrl+C without preventDefault so paste/copy still work", () => {
    expect(targetsShortcutAction({ key: "v", ctrlKey: true, metaKey: false, altKey: false })).toEqual({
      type: "contain",
      stopPropagation: true,
      preventDefault: false,
    });
    expect(targetsShortcutAction({ key: "c", ctrlKey: true, metaKey: false, altKey: false }).type).toBe(
      "contain",
    );
  });

  it("blocks Ctrl+W / Ctrl+Q so the host cannot quit from the Targets field", () => {
    expect(targetsShortcutAction({ key: "w", ctrlKey: true, metaKey: false, altKey: false }).type).toBe(
      "block",
    );
    expect(targetsShortcutAction({ key: "q", ctrlKey: true, metaKey: false, altKey: false }).type).toBe(
      "block",
    );
  });

  it("ignores plain typing", () => {
    expect(targetsShortcutAction({ key: "a", ctrlKey: false, metaKey: false, altKey: false }).type).toBe(
      "ignore",
    );
  });
});

describe("applyTargetsShortcut", () => {
  it("selects all and stops propagation on Ctrl+A", () => {
    const selectAll = vi.fn();
    const e = {
      key: "a",
      ctrlKey: true,
      metaKey: false,
      altKey: false,
      stopPropagation: vi.fn(),
      preventDefault: vi.fn(),
    };
    expect(applyTargetsShortcut(e, selectAll)).toBe(true);
    expect(e.stopPropagation).toHaveBeenCalledOnce();
    expect(e.preventDefault).toHaveBeenCalledOnce();
    expect(selectAll).toHaveBeenCalledOnce();
  });
});
