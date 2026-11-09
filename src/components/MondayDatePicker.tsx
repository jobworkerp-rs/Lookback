import { useState } from "react";
import { useTranslation } from "react-i18next";
import { MONDAY_FIRST_WEEKDAY_KEYS } from "@/lib/calendarWeekdays";
import { buildMonthGrid, dayKey, monthKeyToYearMonth, yearMonthToKey } from "@/lib/summaryPeriod";
import { PickerField } from "./PickerField";
export interface MondayDatePickerProps {
  value: string;
  onChange: (value: string) => void;
  title: string;
  disabled?: boolean;
}

/** A date picker with a fixed Monday-first grid, matching summary periods. */
export function MondayDatePicker({ value, onChange, title, disabled }: MondayDatePickerProps) {
  const { t } = useTranslation();
  const [month, setMonth] = useState(() => monthForDate(dateFromKey(value) ?? new Date()));
  const ym = monthKeyToYearMonth(month);
  const grid = ym ? buildMonthGrid(ym.y, ym.m) : [];

  function shiftMonth(delta: number) {
    if (!ym) return;
    const next = new Date(ym.y, ym.m - 1 + delta, 1);
    setMonth(monthForDate(next));
  }

  return (
    <PickerField
      value={value}
      title={title}
      clearLabel={t("dateInput.clearDate")}
      onClear={() => onChange("")}
      disabled={disabled}
      onOpen={() => setMonth(monthForDate(dateFromKey(value) ?? new Date()))}
    >
      {(close) =>
        ym && (
          <>
            <div className="picker-head">
              <button
                type="button"
                className="btn"
                onClick={() => shiftMonth(-1)}
                title={t("calendar.prevMonth")}
              >
                ‹
              </button>
              <span>{t("calendar.yearMonth", { year: ym.y, month: ym.m })}</span>
              <button
                type="button"
                className="btn"
                onClick={() => shiftMonth(1)}
                title={t("calendar.nextMonth")}
              >
                ›
              </button>
            </div>
            <div className="monday-date-picker-grid">
              {MONDAY_FIRST_WEEKDAY_KEYS.map((key) => (
                <span key={key} className="monday-date-picker-weekday">
                  {t(key)}
                </span>
              ))}
              {grid.map((date, index) =>
                date ? (
                  <button
                    key={dayKey(date)}
                    type="button"
                    className={`picker-cell ${dayKey(date) === value ? "selected" : ""}`}
                    onClick={() => {
                      onChange(dayKey(date));
                      close();
                    }}
                  >
                    {date.getDate()}
                  </button>
                ) : (
                  // biome-ignore lint/suspicious/noArrayIndexKey: fixed grid slot
                  <span key={`blank-${index}`} className="monday-date-picker-blank" />
                ),
              )}
            </div>
          </>
        )
      }
    </PickerField>
  );
}

function dateFromKey(key: string): Date | null {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(key);
  if (!match) return null;
  const date = new Date(Number(match[1]), Number(match[2]) - 1, Number(match[3]));
  return dayKey(date) === key ? date : null;
}

function monthForDate(date: Date): string {
  return yearMonthToKey(date.getFullYear(), date.getMonth() + 1);
}
