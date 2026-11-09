import { useState } from "react";
import { useTranslation } from "react-i18next";
import { monthKeyToYearMonth, yearMonthToKey } from "@/lib/summaryPeriod";
import { PickerField } from "./PickerField";

export interface MonthPickerProps {
  /** `YYYY-MM`, or empty when unset. */
  value: string;
  onChange: (value: string) => void;
  title: string;
  disabled?: boolean;
}

const MONTHS = Array.from({ length: 12 }, (_, i) => i + 1);

/** A year-month picker that reports `YYYY-MM`, the format of monthly periods. */
export function MonthPicker({ value, onChange, title, disabled }: MonthPickerProps) {
  const { t, i18n } = useTranslation();
  const [year, setYear] = useState(() => yearOf(value));
  const selected = monthKeyToYearMonth(value);
  const monthLabel = new Intl.DateTimeFormat(i18n.language, { month: "short" });

  return (
    <PickerField
      value={value}
      title={title}
      clearLabel={t("dateInput.clearMonth")}
      onClear={() => onChange("")}
      disabled={disabled}
      onOpen={() => setYear(yearOf(value))}
    >
      {(close) => (
        <>
          <div className="picker-head">
            <button
              type="button"
              className="btn"
              onClick={() => setYear((y) => y - 1)}
              title={t("calendar.prevYear")}
            >
              ‹
            </button>
            <span>{t("calendar.year", { year })}</span>
            <button
              type="button"
              className="btn"
              onClick={() => setYear((y) => y + 1)}
              title={t("calendar.nextYear")}
            >
              ›
            </button>
          </div>
          <div className="month-picker-grid">
            {MONTHS.map((month) => (
              <button
                key={month}
                type="button"
                className={`picker-cell ${selected?.y === year && selected.m === month ? "selected" : ""}`}
                onClick={() => {
                  onChange(yearMonthToKey(year, month));
                  close();
                }}
              >
                {monthLabel.format(new Date(year, month - 1, 1))}
              </button>
            ))}
          </div>
        </>
      )}
    </PickerField>
  );
}

function yearOf(value: string): number {
  return monthKeyToYearMonth(value)?.y ?? new Date().getFullYear();
}
