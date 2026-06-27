import { useState } from "react";

/**
 * App-level holder for "the Settings tab has unsaved changes". Lives above
 * the page (like `useTheme`) because the Settings page unmounts on a tab
 * switch — a page-local flag would vanish exactly when the leave-guard
 * needs to read it. `App.tsx` reads `dirty` to decide whether to intercept
 * a navigation away from Settings; `Settings` calls `setDirty` from its
 * aggregate dirty-count effect and clears it on unmount.
 */
export interface SettingsDirtyControl {
  dirty: boolean;
  setDirty: (v: boolean) => void;
}

export function useSettingsDirty(): SettingsDirtyControl {
  const [dirty, setDirty] = useState(false);
  return { dirty, setDirty };
}
