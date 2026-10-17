/**
 * Key handling for the Targets textarea.
 *
 * WebKitGTK (Tauri on Linux) has been observed to tear down the whole
 * webview process on some Ctrl/Cmd+A / Ctrl+V paths when the event also
 * bubbles into host chrome. We stop propagation for all modifier
 * shortcuts originating in the Targets field, and implement Select-All
 * ourselves so the host never sees Ctrl+A.
 */

export type TargetsKeyAction =
  | { type: "selectAll"; stopPropagation: true; preventDefault: true }
  | { type: "block"; stopPropagation: true; preventDefault: true }
  | { type: "contain"; stopPropagation: true; preventDefault: false }
  | { type: "ignore" };

export function targetsShortcutAction(e: {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  altKey: boolean;
}): TargetsKeyAction {
  const mod = e.ctrlKey || e.metaKey;
  if (!mod || e.altKey) return { type: "ignore" };
  const k = e.key.length === 1 ? e.key.toLowerCase() : e.key;
  if (k === "a") {
    return { type: "selectAll", stopPropagation: true, preventDefault: true };
  }
  // Host quit / new-window chords — never let them leave the field.
  if (k === "w" || k === "q" || k === "n" || k === "t") {
    return { type: "block", stopPropagation: true, preventDefault: true };
  }
  // Cut / copy / paste / undo / redo: browser default, but contained.
  if (k === "c" || k === "v" || k === "x" || k === "z" || k === "y") {
    return { type: "contain", stopPropagation: true, preventDefault: false };
  }
  return { type: "contain", stopPropagation: true, preventDefault: false };
}

/** Apply the action to a DOM-like key event. Returns true if handled. */
export function applyTargetsShortcut(
  e: {
    key: string;
    ctrlKey: boolean;
    metaKey: boolean;
    altKey: boolean;
    stopPropagation: () => void;
    preventDefault: () => void;
  },
  selectAll: () => void,
): boolean {
  const action = targetsShortcutAction(e);
  if (action.type === "ignore") return false;
  if (action.stopPropagation) e.stopPropagation();
  if (action.preventDefault) e.preventDefault();
  if (action.type === "selectAll") selectAll();
  return true;
}
