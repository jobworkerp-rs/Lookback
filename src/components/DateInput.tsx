import { useTranslation } from "react-i18next";

export type DateInputType = "date" | "month" | "week";

export interface DateInputProps {
  value: string;
  onChange: (value: string) => void;
  /** Native picker granularity: "date" (YYYY-MM-DD, default), "month"
   *  (YYYY-MM), or "week" (YYYY-Www). The value format follows the type. */
  type?: DateInputType;
  title?: string;
  disabled?: boolean;
  className?: string;
}

/** i18n key per granularity for the clear-button label, resolved with `t`. */
const CLEAR_LABEL_KEY: Record<DateInputType, string> = {
  date: "dateInput.clearDate",
  month: "dateInput.clearMonth",
  week: "dateInput.clearWeek",
};

/**
 * `<input type="date|month|week">` wrapper that makes the unselected state
 * legible. A native empty date input renders its placeholder (年/月/日) in the
 * regular text color, so it looks identical to a real selection. We dim the
 * field while empty and expose a clear button once a value is set. The `type`
 * switches the picker granularity for the staged-generate range pickers.
 */
export function DateInput({
  value,
  onChange,
  type = "date",
  title,
  disabled,
  className = "text-input",
}: DateInputProps) {
  const { t } = useTranslation();
  const hasValue = value.length > 0;
  // `has-value` flips the field from the "empty" CSS state to normal rendering.
  // filter(Boolean) keeps the class list clean when `className` is "" (callers
  // that don't want the base `text-input` style, e.g. ImportDialog).
  const inputClass = [className, hasValue && "has-value"].filter(Boolean).join(" ");
  const clearLabel = t(CLEAR_LABEL_KEY[type]);
  return (
    <span className="date-input">
      <input
        type={type}
        className={inputClass}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        title={title}
        disabled={disabled}
      />
      {hasValue && !disabled && (
        <button
          type="button"
          className="date-input-clear"
          onClick={() => onChange("")}
          title={clearLabel}
          aria-label={clearLabel}
        >
          ×
        </button>
      )}
    </span>
  );
}
