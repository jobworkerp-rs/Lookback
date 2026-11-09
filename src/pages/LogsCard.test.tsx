import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { LogsCard } from "./Settings";

const readSidecarLog = vi.fn();

vi.mock("@/api", () => ({
  readSidecarLog: (...args: unknown[]) => readSidecarLog(...args),
}));

function renderCard() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <LogsCard />
      </QueryClientProvider>
    </I18nextProvider>,
  );
}

describe("LogsCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    readSidecarLog.mockReset();
    readSidecarLog.mockResolvedValue({
      file_name: "app.log",
      content: "",
      truncated: false,
      file_size: 0,
    });
  });

  it("reads the Issue Tracker stderr log when selected", async () => {
    renderCard();
    await waitFor(() => expect(readSidecarLog).toHaveBeenCalledWith("app", "stdout"));

    fireEvent.click(screen.getByRole("button", { name: "issue-tracker" }));
    await waitFor(() => expect(readSidecarLog).toHaveBeenCalledWith("issue_tracker", "stdout"));

    fireEvent.click(screen.getByRole("button", { name: "stderr" }));
    await waitFor(() => expect(readSidecarLog).toHaveBeenCalledWith("issue_tracker", "stderr"));
  });
});
