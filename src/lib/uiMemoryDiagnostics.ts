const ACTIVITY_SAMPLE_COMMAND = "report_ui_activity_sample";
const SAMPLE_INTERVAL_MS = 60_000;
const MAX_COUNTER_VALUE = Number.MAX_SAFE_INTEGER;

export interface UiActivityCounters {
  staged_received_total: number;
  staged_published_total: number;
  generated_refresh_total: number;
  summaries_invalidated_total: number;
  summary_list_render_attempts_total: number;
  summary_card_render_attempts_total: number;
}

export type UiActivityCounter = keyof UiActivityCounters;

type UiActivityInvoke = (
  command: typeof ACTIVITY_SAMPLE_COMMAND,
  args: { counters: UiActivityCounters },
) => Promise<boolean>;

const counters: UiActivityCounters = {
  staged_received_total: 0,
  staged_published_total: 0,
  generated_refresh_total: 0,
  summaries_invalidated_total: 0,
  summary_list_render_attempts_total: 0,
  summary_card_render_attempts_total: 0,
};

export function incrementUiMemoryCounter(counter: UiActivityCounter, amount = 1): void {
  if (!Number.isSafeInteger(amount) || amount <= 0) return;
  counters[counter] = Math.min(MAX_COUNTER_VALUE, counters[counter] + amount);
}

export function installUiMemoryDiagnostics(invoke: UiActivityInvoke): { dispose: () => void } {
  let disposed = false;
  let inFlight = false;
  let interval: ReturnType<typeof setInterval> | null = null;

  const sendSample = (initial: boolean) => {
    if (disposed || inFlight) return;

    inFlight = true;
    let delivery: Promise<boolean>;
    try {
      delivery = Promise.resolve(invoke(ACTIVITY_SAMPLE_COMMAND, { counters: copyCounters() }));
    } catch {
      inFlight = false;
      return;
    }

    void delivery.then(
      (supported) => {
        inFlight = false;
        if (!initial || !supported || disposed) return;
        try {
          interval = setInterval(() => sendSample(false), SAMPLE_INTERVAL_MS);
        } catch {
          // Sampling is diagnostic-only and must not interfere with the UI.
        }
      },
      () => {
        inFlight = false;
      },
    );
  };

  sendSample(true);

  return {
    dispose() {
      if (disposed) return;
      disposed = true;
      if (interval !== null) {
        clearInterval(interval);
        interval = null;
      }
    },
  };
}

function copyCounters(): UiActivityCounters {
  return { ...counters };
}
