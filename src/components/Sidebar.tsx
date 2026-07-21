import { useTranslation } from "react-i18next";
import type { LocaleControl } from "@/hooks/useLocale";
import type { SidecarStatus } from "@/hooks/useSidecarStatus";
import type { ThemeControl } from "@/hooks/useTheme";
import { LOCALE_OPTIONS } from "@/lib/locale";
import { THEME_OPTIONS } from "@/lib/theme";

export type Route =
  | "threads"
  | "summaries"
  | "reflections"
  | "personality"
  | "chat"
  | "periodic"
  | "settings";

export interface SidebarProps {
  current: Route;
  onChange: (route: Route) => void;
  threadCount: number | null;
  threadCountTruncated?: boolean;
  summaryCount: number | null;
  sidecar: SidecarStatus;
  theme: ThemeControl;
  locale: LocaleControl;
}

interface NavEntry {
  key: Exclude<Route, "settings" | "periodic">;
  labelKey: string;
}

const BROWSE_ENTRIES: NavEntry[] = [
  { key: "threads", labelKey: "nav.threads" },
  { key: "summaries", labelKey: "nav.summaries" },
  { key: "reflections", labelKey: "nav.reflections" },
  { key: "personality", labelKey: "nav.personality" },
  { key: "chat", labelKey: "nav.chat" },
];

export function Sidebar({
  current,
  onChange,
  threadCount,
  threadCountTruncated = false,
  summaryCount,
  sidecar,
  theme,
  locale,
}: SidebarProps) {
  const { t } = useTranslation();
  const badges: Partial<Record<Route, string>> = {
    threads:
      threadCount != null
        ? threadCountTruncated
          ? `${threadCount}+`
          : `${threadCount}`
        : undefined,
    summaries: summaryCount != null ? `${summaryCount}` : undefined,
  };
  return (
    <aside className="sidebar">
      <div className="brand">Lookback</div>

      <div className="nav-section">{t("nav.section.browse")}</div>
      {BROWSE_ENTRIES.map((entry) => (
        <button
          key={entry.key}
          type="button"
          className={`nav-item ${current === entry.key ? "active" : ""}`}
          onClick={() => onChange(entry.key)}
        >
          <span>{t(entry.labelKey)}</span>
          {badges[entry.key] && <span className="badge">{badges[entry.key]}</span>}
        </button>
      ))}

      <div className="nav-section">{t("nav.section.system")}</div>
      <button
        type="button"
        className={`nav-item ${current === "periodic" ? "active" : ""}`}
        onClick={() => onChange("periodic")}
      >
        <span>{t("nav.periodic")}</span>
      </button>
      <button
        type="button"
        className={`nav-item ${current === "settings" ? "active" : ""}`}
        onClick={() => onChange("settings")}
      >
        <span>{t("nav.settings")}</span>
      </button>

      {/* Theme is a display-only, instantly-applied preference, so it lives
          here as a compact toggle rather than as a large Settings card. */}
      <div className="sidebar-theme">
        <span className="sidebar-theme-label">{t("theme.label")}</span>
        <div className="segment">
          {THEME_OPTIONS.map((opt) => (
            <button
              key={opt.value}
              type="button"
              className={`segment-btn${theme.pref === opt.value ? " active" : ""}`}
              onClick={() => theme.setPref(opt.value)}
            >
              {t(opt.labelKey)}
            </button>
          ))}
        </div>
      </div>

      {/* Language is the locale analogue of the theme toggle: another
          instantly-applied display preference, so it sits right below it. */}
      <div className="sidebar-theme">
        <span className="sidebar-theme-label">{t("locale.label")}</span>
        <div className="segment">
          {LOCALE_OPTIONS.map((opt) => (
            <button
              key={opt.value}
              type="button"
              className={`segment-btn${locale.pref === opt.value ? " active" : ""}`}
              onClick={() => locale.setPref(opt.value)}
            >
              {t(opt.labelKey)}
            </button>
          ))}
        </div>
      </div>

      <SidecarStatusPanel status={sidecar} />
    </aside>
  );
}

function SidecarStatusPanel({ status }: { status: SidecarStatus }) {
  const { t } = useTranslation();
  const phaseLabel = t(`sidecar.phase.${status.phase}`);
  const dotClass = PHASE_DOT[status.phase];
  return (
    <div className="sidecar-status">
      <div style={{ fontWeight: 600 }}>{t("sidecar.title")}</div>
      <div className="sidecar-row">
        <span className={`dot ${dotClass}`} />
        <span>jobworkerp</span>
        <span className="port">
          {status.endpoints ? `:${status.endpoints.jobworkerp_port}` : phaseLabel}
        </span>
      </div>
      <div className="sidecar-row">
        <span className={`dot ${dotClass}`} />
        <span>memories</span>
        <span className="port">
          {status.endpoints?.memories_port != null
            ? `:${status.endpoints.memories_port}`
            : phaseLabel}
        </span>
      </div>
      <div className="sidecar-row">
        <span className={`dot ${dotClass}`} />
        <span>conductor</span>
        <span className="port">
          {status.endpoints ? `:${status.endpoints.conductor_port}` : phaseLabel}
        </span>
      </div>
      {/* No phase=error branch here: when the sidecar fails to start
          App.tsx unconditionally swaps to the BootError full-screen
          surface, so Sidebar never mounts in that state. */}
      {status.warnings.map((w) => (
        <div key={`${w.kind}:${w.detail ?? ""}`} className="sidecar-warning">
          <span className="dot warn" />
          <span className="sidecar-warning-label">{t(`sidecar.warning.${w.kind}`, w.kind)}</span>
          <span className="sidecar-warning-msg" title={w.detail ?? undefined}>
            {w.message}
          </span>
        </div>
      ))}
    </div>
  );
}

const PHASE_DOT: Record<SidecarStatus["phase"], string> = {
  ready: "ok",
  error: "err",
  starting: "warn",
};
