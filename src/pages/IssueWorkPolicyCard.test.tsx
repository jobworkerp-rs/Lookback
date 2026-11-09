import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { IssueWorkPolicyCard } from "./IssueWorkPolicyCard";

const getWorkEligibility = vi.fn();
const setWorkEligibility = vi.fn();

vi.mock("@/api", () => ({
  getWorkEligibility: () => getWorkEligibility(),
  setWorkEligibility: (request: unknown) => setWorkEligibility(request),
}));

function renderCard() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <IssueWorkPolicyCard />
      </QueryClientProvider>
    </I18nextProvider>,
  );
}

describe("IssueWorkPolicyCard", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    i18n.changeLanguage("en");
    getWorkEligibility.mockResolvedValue({ work_eligibility: "REVIEWED_ONLY" });
    setWorkEligibility.mockResolvedValue({ work_eligibility: "INCLUDE_AUTO_REGISTERED" });
  });

  it("persists the selected work-eligibility mode", async () => {
    renderCard();
    expect(
      screen.getByText(
        "This setting determines which unreviewed auto-registered Issues appear in the Issue list and board, and which Issues agents can start work on. If the change fails, the current setting remains in use.",
      ),
    ).toBeInTheDocument();
    const select = await screen.findByLabelText("Review status for work");
    fireEvent.change(select, { target: { value: "INCLUDE_AUTO_REGISTERED" } });

    await waitFor(() =>
      expect(setWorkEligibility).toHaveBeenCalledWith({
        work_eligibility: "INCLUDE_AUTO_REGISTERED",
      }),
    );
  });
});
