import { fireEvent, screen } from "@testing-library/react";
import { useState } from "react";
import { beforeEach, describe, expect, it } from "vitest";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { useSettingsDirty } from "./useSettingsDirty";

// Reproduces App.tsx's leave-guard wiring in isolation: a dirty Settings
// tab parks an attempted navigation behind a confirm dialog; "破棄して移動"
// clears the dirty flag and navigates, "キャンセル" stays put. Mounting the
// full App would require mocking ~15 modules, so this harness pins the
// guard contract against the real useSettingsDirty hook + ConfirmDialog.
type Tab = "settings" | "threads";

function Harness() {
  const dirty = useSettingsDirty();
  const [route, setRoute] = useState<Tab>("settings");
  const [pending, setPending] = useState<Tab | null>(null);

  const guardedSetRoute = (next: Tab) => {
    if (route === "settings" && next !== "settings" && dirty.dirty) {
      setPending(next);
    } else {
      setRoute(next);
    }
  };

  return (
    <div>
      <div data-testid="route">{route}</div>
      <button type="button" onClick={() => dirty.setDirty(true)}>
        make-dirty
      </button>
      <button type="button" onClick={() => guardedSetRoute("threads")}>
        go-threads
      </button>
      {pending && (
        <ConfirmDialog
          title="保存していない変更があります"
          message="設定に未保存の変更があります。破棄して移動しますか?"
          confirmLabel="破棄して移動"
          onConfirm={() => {
            dirty.setDirty(false);
            setRoute(pending);
            setPending(null);
          }}
          onCancel={() => setPending(null)}
        />
      )}
    </div>
  );
}

describe("settings leave-guard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("navigates immediately when the tab is clean", () => {
    renderWithProviders(<Harness />);
    fireEvent.click(screen.getByText("go-threads"));
    expect(screen.getByTestId("route").textContent).toBe("threads");
    expect(screen.queryByText("保存していない変更があります")).toBeNull();
  });

  it("intercepts navigation while dirty and stays put on キャンセル", () => {
    renderWithProviders(<Harness />);
    fireEvent.click(screen.getByText("make-dirty"));
    fireEvent.click(screen.getByText("go-threads"));
    // Dialog shown, still on settings.
    expect(screen.getByText("保存していない変更があります")).toBeInTheDocument();
    expect(screen.getByTestId("route").textContent).toBe("settings");
    fireEvent.click(screen.getByText("キャンセル"));
    expect(screen.queryByText("保存していない変更があります")).toBeNull();
    expect(screen.getByTestId("route").textContent).toBe("settings");
  });

  it("discards and navigates on 破棄して移動", () => {
    renderWithProviders(<Harness />);
    fireEvent.click(screen.getByText("make-dirty"));
    fireEvent.click(screen.getByText("go-threads"));
    fireEvent.click(screen.getByText("破棄して移動"));
    expect(screen.getByTestId("route").textContent).toBe("threads");
    expect(screen.queryByText("保存していない変更があります")).toBeNull();
  });
});
