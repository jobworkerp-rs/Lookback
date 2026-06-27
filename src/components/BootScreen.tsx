import type { ReactNode } from "react";

interface BootScreenProps {
  /** Optional "Lookback" branded heading (omitted on plain error screens). */
  title?: string;
  detail: string;
  /** Renders the detail in the danger tone. */
  error?: boolean;
  /** Optional action(s) shown below the detail (e.g. a retry button). */
  action?: ReactNode;
}

/** Full-screen centered status surface shown before the main app mounts:
 *  sidecar startup, the setup-status probe, and their error fallbacks. */
export function BootScreen({ title, detail, error, action }: BootScreenProps) {
  return (
    <div className="boot-screen">
      {title && <div className="boot-screen-title">{title}</div>}
      <div className={error ? "boot-screen-error" : "boot-screen-detail"}>{detail}</div>
      {action}
    </div>
  );
}
