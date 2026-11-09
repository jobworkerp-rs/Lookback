import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";
import React from "react";
import ReactDOM from "react-dom/client";
import { I18nextProvider } from "react-i18next";
import { App } from "./App";
import { AppCloseController } from "./components/AppCloseController";
import i18n from "./i18n";
import { installReactPerformanceTimingCleanup } from "./lib/reactPerformanceTimingCleanup";
import { installUiDiagnostics } from "./lib/uiDiagnostics";
import { installUiMemoryDiagnostics } from "./lib/uiMemoryDiagnostics";
import "./styles/tokens.css";
import "./styles/app.css";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      refetchOnWindowFocus: false,
      staleTime: 30_000,
    },
  },
});

const root = document.getElementById("root");
if (!root) throw new Error("missing #root element");

const uiDiagnostics = installUiDiagnostics(window, (command, args) => invoke<void>(command, args));
const uiMemoryDiagnostics = installUiMemoryDiagnostics((command, args) =>
  invoke<boolean>(command, args),
);
const reactTimingCleanup = import.meta.env.DEV
  ? installReactPerformanceTimingCleanup(window)
  : undefined;

if (import.meta.hot) {
  import.meta.hot.dispose(() => {
    reactTimingCleanup?.dispose();
    uiMemoryDiagnostics.dispose();
    uiDiagnostics.dispose();
  });
}

ReactDOM.createRoot(root, uiDiagnostics.rootOptions).render(
  <React.StrictMode>
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={queryClient}>
        {[<App key="app" />, <AppCloseController key="close-choice-controller" />]}
      </QueryClientProvider>
    </I18nextProvider>
  </React.StrictMode>,
);
