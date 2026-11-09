// React development builds record every component render and scheduler step
// with `performance.measure` so DevTools can draw its "⚛" tracks. Nothing
// consumes those entries afterwards, so a long-running dev session keeps
// growing the renderer's performance timeline. This cleanup drops them as
// they arrive. A name is cleared only when every entry carrying it belongs to
// React, because `clearMeasures` works by name and must not delete measures
// recorded by our own diagnostics or other tools.

export const REACT_COMPONENTS_TRACK = "Components ⚛";
export const REACT_SCHEDULER_TRACK_GROUP = "Scheduler ⚛";

export interface ReactPerformanceTimingCleanupHost {
  performance?: Pick<Performance, "getEntriesByName" | "clearMeasures">;
  PerformanceObserver?: typeof PerformanceObserver;
}

export function installReactPerformanceTimingCleanup(host?: ReactPerformanceTimingCleanupHost): {
  dispose: () => void;
} {
  let disposed = false;
  let observer: PerformanceObserver | undefined;

  const dispose = () => {
    if (disposed) return;
    disposed = true;
    try {
      observer?.disconnect();
    } catch {
      // Cleanup is diagnostic-only and must not interfere with the UI.
    }
  };

  const performance = host?.performance;
  const Observer = host?.PerformanceObserver;
  if (
    typeof performance?.getEntriesByName !== "function" ||
    typeof performance.clearMeasures !== "function" ||
    typeof Observer !== "function"
  ) {
    return { dispose };
  }

  try {
    observer = new Observer((list) => {
      if (disposed) return;
      const names = new Set<string>();
      for (const entry of list.getEntries()) {
        if (isReactMeasure(entry)) names.add(entry.name);
      }
      for (const name of names) {
        try {
          const retained = performance.getEntriesByName(name, "measure");
          if (retained.length > 0 && retained.every(isReactMeasure)) {
            performance.clearMeasures(name);
          }
        } catch {
          // A failed lookup or clear must not stop the remaining names.
        }
      }
    });
    observer.observe({ type: "measure", buffered: true });
  } catch {
    dispose();
  }

  return { dispose };
}

function isReactMeasure(entry: PerformanceEntry): boolean {
  try {
    const devtools = (entry as PerformanceMeasure).detail?.devtools;
    return (
      devtools?.track === REACT_COMPONENTS_TRACK ||
      devtools?.trackGroup === REACT_SCHEDULER_TRACK_GROUP
    );
  } catch {
    // Entry details are arbitrary user data; an unreadable one is not React's.
    return false;
  }
}
