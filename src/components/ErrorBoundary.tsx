import { Component, type ErrorInfo, type ReactNode } from "react";
import { useTranslation } from "react-i18next";

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

/**
 * Fallback view extracted as a function component so it can call
 * `useTranslation`: the class boundary itself can't use hooks, but its
 * rendered fallback can.
 */
function ErrorFallback({ message, onReset }: { message: string; onReset: () => void }) {
  const { t } = useTranslation();
  return (
    <div className="error-boundary">
      <div className="empty-title">{t("errorBoundary.title")}</div>
      <div className="empty-desc">{message}</div>
      <button type="button" className="btn primary" onClick={onReset}>
        {t("common.reload")}
      </button>
    </div>
  );
}

/**
 * Catches render-time exceptions in its subtree and shows a fallback instead
 * of letting the throw unmount the whole app (React's default for an uncaught
 * render error). Wrap the routed page area keyed by route so a crash in one
 * tab leaves the sidebar usable and navigating away/back resets the boundary.
 *
 * Must be a class component: `getDerivedStateFromError` / `componentDidCatch`
 * have no hook equivalent.
 */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("Unhandled render error:", error, info.componentStack);
  }

  private reset = () => this.setState({ error: null });

  render() {
    if (this.state.error) {
      return <ErrorFallback message={this.state.error.message} onReset={this.reset} />;
    }
    return this.props.children;
  }
}
