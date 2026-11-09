import { describe, expect, it, vi } from "vitest";
import {
  installReactPerformanceTimingCleanup,
  REACT_COMPONENTS_TRACK,
  REACT_SCHEDULER_TRACK_GROUP,
  type ReactPerformanceTimingCleanupHost,
} from "./reactPerformanceTimingCleanup";

interface TestEntry {
  name: string;
  detail?: unknown;
}

type TestCallback = (list: { getEntries(): readonly TestEntry[] }) => void;

function reactEntry(name: string, kind: "component" | "scheduler" = "component"): TestEntry {
  return {
    name,
    detail: {
      devtools:
        kind === "component"
          ? { track: REACT_COMPONENTS_TRACK }
          : { trackGroup: REACT_SCHEDULER_TRACK_GROUP },
    },
  };
}

function otherEntry(name: string): TestEntry {
  return { name, detail: { devtools: { track: "Other" } } };
}

function makeHost() {
  const measures = new Map<string, TestEntry[]>();
  const observers: Array<{
    callback: TestCallback;
    observe: ReturnType<typeof vi.fn>;
    disconnect: ReturnType<typeof vi.fn>;
  }> = [];
  const performance = {
    // The receiver checks mirror native methods, which throw when unbound.
    getEntriesByName: vi.fn(function (this: unknown, name: string, type?: string) {
      if (this !== performance) throw new Error("unbound getEntriesByName");
      if (type !== "measure") throw new Error("unexpected entry type");
      return [...(measures.get(name) ?? [])];
    }),
    clearMeasures: vi.fn(function (this: unknown, name?: string) {
      if (this !== performance) throw new Error("unbound clearMeasures");
      if (name === undefined) throw new Error("unscoped clear is forbidden");
      measures.delete(name);
    }),
  };
  class FakePerformanceObserver {
    readonly observe = vi.fn();
    readonly disconnect = vi.fn();

    constructor(readonly callback: TestCallback) {
      observers.push(this);
    }
  }
  const host = {
    performance,
    PerformanceObserver: FakePerformanceObserver,
  } as unknown as ReactPerformanceTimingCleanupHost;

  return {
    host,
    performance,
    measures,
    observers,
    add(...entries: TestEntry[]) {
      for (const entry of entries) {
        measures.set(entry.name, [...(measures.get(entry.name) ?? []), entry]);
      }
    },
    deliver(entries: readonly TestEntry[]) {
      observers[0]?.callback({ getEntries: () => entries });
    },
  };
}

describe("React performance timing cleanup", () => {
  it("observes buffered measures and clears React component and scheduler entries", () => {
    const fake = makeHost();
    const cleanup = installReactPerformanceTimingCleanup(fake.host);

    expect(fake.observers[0]?.observe).toHaveBeenCalledExactlyOnceWith({
      type: "measure",
      buffered: true,
    });
    const component = reactEntry("component");
    const scheduler = reactEntry("scheduler", "scheduler");
    fake.add(component, scheduler);
    fake.deliver([component, scheduler]);

    expect(fake.performance.clearMeasures).toHaveBeenCalledTimes(2);
    expect(fake.performance.clearMeasures).toHaveBeenCalledWith("component");
    expect(fake.performance.clearMeasures).toHaveBeenCalledWith("scheduler");
    expect(fake.measures.size).toBe(0);
    cleanup.dispose();
  });

  it("preserves ordinary, unknown, and malformed timing details", () => {
    const fake = makeHost();
    const cleanup = installReactPerformanceTimingCleanup(fake.host);
    const entries = [
      otherEntry("ordinary"),
      { name: "unknown", detail: { devtools: { track: "Components", trackGroup: "Scheduler" } } },
      { name: "malformed", detail: null },
      { name: "no-detail" },
    ];
    fake.add(...entries);
    fake.deliver(entries);

    expect(fake.measures.size).toBe(4);
    expect(fake.performance.clearMeasures).not.toHaveBeenCalled();
    cleanup.dispose();
  });

  it.each([
    ["before", true],
    ["after", false],
  ])("keeps a name shared with a non-React measure recorded %s the React one", (_when, first) => {
    const fake = makeHost();
    const cleanup = installReactPerformanceTimingCleanup(fake.host);
    const known = reactEntry("collision");
    const other = otherEntry("collision");
    fake.add(...(first ? [other, known] : [known, other]));
    fake.deliver([known]);

    expect(fake.measures.get("collision")).toEqual(first ? [other, known] : [known, other]);
    expect(fake.performance.clearMeasures).not.toHaveBeenCalled();
    cleanup.dispose();
  });

  it("clears a shared name when every retained entry is React's", () => {
    const fake = makeHost();
    const cleanup = installReactPerformanceTimingCleanup(fake.host);
    const first = reactEntry("same");
    const second = reactEntry("same", "scheduler");
    fake.add(first, second);
    fake.deliver([first, second]);

    expect(fake.performance.clearMeasures).toHaveBeenCalledExactlyOnceWith("same");
    expect(fake.measures.has("same")).toBe(false);
    cleanup.dispose();
  });

  it("no-ops when required APIs are absent or the observer cannot start", () => {
    expect(() => installReactPerformanceTimingCleanup()).not.toThrow();

    const missingObserver = { performance: makeHost().performance };
    expect(() =>
      installReactPerformanceTimingCleanup(
        missingObserver as unknown as ReactPerformanceTimingCleanupHost,
      ),
    ).not.toThrow();

    for (const method of ["getEntriesByName", "clearMeasures"] as const) {
      const missing = makeHost();
      missing.performance[method] = undefined as never;
      expect(() => installReactPerformanceTimingCleanup(missing.host)).not.toThrow();
    }

    const constructorFailure = makeHost();
    constructorFailure.host.PerformanceObserver = class {
      constructor() {
        throw new Error("unsupported");
      }
    } as unknown as typeof PerformanceObserver;
    expect(() => installReactPerformanceTimingCleanup(constructorFailure.host)).not.toThrow();

    const observeFailure = makeHost();
    const disconnect = vi.fn();
    observeFailure.host.PerformanceObserver = class {
      observe() {
        throw new Error("unsupported");
      }
      disconnect = disconnect;
    } as unknown as typeof PerformanceObserver;
    const failed = installReactPerformanceTimingCleanup(observeFailure.host);
    expect(() => failed.dispose()).not.toThrow();
    expect(disconnect).toHaveBeenCalledTimes(1);
  });

  it("skips unreadable details and failed lookups or clears while processing other names", () => {
    const fake = makeHost();
    const cleanup = installReactPerformanceTimingCleanup(fake.host);
    const unreadable = reactEntry("unreadable");
    Object.defineProperty(unreadable, "detail", {
      get() {
        throw new Error("private detail failure");
      },
    });
    const lookupFailure = reactEntry("lookup-failure");
    const clearFailure = reactEntry("clear-failure");
    const success = reactEntry("success");
    fake.add(unreadable, lookupFailure, clearFailure, success);
    const lookup = fake.performance.getEntriesByName.getMockImplementation();
    const clear = fake.performance.clearMeasures.getMockImplementation();
    fake.performance.getEntriesByName.mockImplementation(function (this: unknown, name, type) {
      if (name === "lookup-failure") throw new Error("lookup failure");
      return lookup?.call(this, name, type) ?? [];
    });
    fake.performance.clearMeasures.mockImplementation(function (this: unknown, name) {
      if (name === "clear-failure") throw new Error("clear failure");
      clear?.call(this, name);
    });

    expect(() => fake.deliver([unreadable, lookupFailure, clearFailure, success])).not.toThrow();
    expect([...fake.measures.keys()]).toEqual(["unreadable", "lookup-failure", "clear-failure"]);
    cleanup.dispose();
  });

  it("disconnects once and ignores callbacks already queued when disposed", () => {
    const fake = makeHost();
    const cleanup = installReactPerformanceTimingCleanup(fake.host);
    cleanup.dispose();
    cleanup.dispose();
    fake.add(reactEntry("queued"));
    fake.deliver([reactEntry("queued")]);

    expect(fake.performance.getEntriesByName).not.toHaveBeenCalled();
    expect(fake.measures.has("queued")).toBe(true);
    expect(fake.observers[0]?.disconnect).toHaveBeenCalledTimes(1);
  });

  it("keeps the timeline empty across repeated batches", () => {
    const fake = makeHost();
    const cleanup = installReactPerformanceTimingCleanup(fake.host);

    for (let batch = 0; batch < 3; batch += 1) {
      const entries = Array.from({ length: 100 }, (_, index) =>
        reactEntry(`batch-${batch}-${index}`, index % 2 === 0 ? "component" : "scheduler"),
      );
      fake.add(...entries);
      fake.deliver(entries);
      expect(fake.measures.size).toBe(0);
    }
    expect(fake.performance.clearMeasures).toHaveBeenCalledTimes(300);
    cleanup.dispose();
  });
});
