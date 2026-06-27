import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import {
  buildMonthGrid,
  epochMsToDailyKey,
  isoWeekOfDate,
  monthKeyToYearMonth,
  yearMonthToKey,
} from "@/lib/summaryPeriod";
import type { SummaryKind } from "@/types/api";

type CalendarKind = Exclude<SummaryKind, "per-thread">;

// ISO weekday order (Mon-first). i18n keys are resolved with `t` at render so
// the header row localizes; the order is fixed regardless of locale.
const WEEKDAY_KEYS = [
  "calendar.weekdayMon",
  "calendar.weekdayTue",
  "calendar.weekdayWed",
  "calendar.weekdayThu",
  "calendar.weekdayFri",
  "calendar.weekdaySat",
  "calendar.weekdaySun",
];

export interface SummaryCalendarProps {
  kind: CalendarKind;
  /** Displayed month, "YYYY-MM". */
  month: string;
  /** period_keys that have a summary (from list_summary_period_keys). */
  periodKeys: string[];
  /** Selected period_key, driving the detail panel and cell highlight. */
  selectedKey: string | null;
  onSelectKey: (key: string) => void;
  onMonthChange: (month: string) => void;
}

/** Month-grid calendar that dots the days/weeks/months that have a summary,
 *  and reports the clicked period_key so the page can show its detail. */
export function SummaryCalendar({
  kind,
  month,
  periodKeys,
  selectedKey,
  onSelectKey,
  onMonthChange,
}: SummaryCalendarProps) {
  const { t } = useTranslation();
  // Memoized so the derived `grid` has a stable dependency: re-parsing
  // `month` each render would yield a fresh object and defeat the memo.
  const ym = useMemo(() => monthKeyToYearMonth(month), [month]);
  const grid = useMemo(() => (ym ? buildMonthGrid(ym.y, ym.m) : []), [ym]);
  const keySet = useMemo(() => new Set(periodKeys), [periodKeys]);

  if (!ym) return null;

  const hasMonth = kind === "monthly" && keySet.has(month);

  function shiftMonth(delta: number) {
    if (!ym) return;
    const d = new Date(ym.y, ym.m - 1 + delta, 1);
    onMonthChange(yearMonthToKey(d.getFullYear(), d.getMonth() + 1));
  }

  return (
    <div className="sum-calendar">
      <div className="sum-cal-head">
        <button
          type="button"
          className="btn"
          onClick={() => shiftMonth(-1)}
          title={t("calendar.prevMonth")}
        >
          ‹
        </button>
        <span className="sum-cal-title">
          {t("calendar.yearMonth", { year: ym.y, month: ym.m })}
        </span>
        <button
          type="button"
          className="btn"
          onClick={() => shiftMonth(1)}
          title={t("calendar.nextMonth")}
        >
          ›
        </button>
        {kind === "monthly" && (
          <button
            type="button"
            className={`segment-btn ${selectedKey === month ? "active" : ""}`}
            disabled={!hasMonth}
            onClick={() => onSelectKey(month)}
            style={{ marginLeft: "auto" }}
          >
            {hasMonth ? t("calendar.monthSummary") : t("calendar.noSummary")}
          </button>
        )}
      </div>

      <div className="sum-cal-grid">
        {WEEKDAY_KEYS.map((wk) => (
          <div key={wk} className="sum-cal-weekday">
            {t(wk)}
          </div>
        ))}
        {grid.map((cell, i) => {
          if (cell == null) {
            // Leading blanks are fixed positions in a freshly built grid.
            // biome-ignore lint/suspicious/noArrayIndexKey: stable grid slot
            return <div key={`blank-${i}`} className="sum-cal-cell blank" />;
          }
          return (
            <CalendarCell
              key={epochMsToDailyKey(cell.getTime())}
              cell={cell}
              kind={kind}
              monthKey={month}
              keySet={keySet}
              selectedKey={selectedKey}
              onSelectKey={onSelectKey}
            />
          );
        })}
      </div>
    </div>
  );
}

interface CalendarCellProps {
  cell: Date;
  kind: CalendarKind;
  monthKey: string;
  keySet: Set<string>;
  selectedKey: string | null;
  onSelectKey: (key: string) => void;
}

/** A single day cell. The period_key it maps to depends on the granularity:
 *  the day itself (daily), its ISO week (weekly), or the shown month
 *  (monthly). Monthly cells are inert — the whole month is selected via the
 *  header button — so they render as a plain div. */
function CalendarCell({
  cell,
  kind,
  monthKey,
  keySet,
  selectedKey,
  onSelectKey,
}: CalendarCellProps) {
  const day = cell.getDate();
  const periodKey =
    kind === "daily"
      ? epochMsToDailyKey(cell.getTime())
      : kind === "weekly"
        ? isoWeekOfDate(cell)
        : monthKey;
  const has = keySet.has(periodKey);

  if (kind === "monthly") {
    return (
      <div className={`sum-cal-cell month ${has ? "has" : "empty"}`}>
        <span className="sum-cal-day">{day}</span>
      </div>
    );
  }

  const selected = selectedKey === periodKey;
  return (
    <button
      type="button"
      className={`sum-cal-cell ${has ? "has" : "empty"} ${selected ? "selected" : ""}`}
      disabled={!has}
      onClick={() => onSelectKey(periodKey)}
      title={has && kind === "weekly" ? periodKey : undefined}
    >
      <span className="sum-cal-day">{day}</span>
      {has && <span className="sum-cal-dot" />}
    </button>
  );
}
