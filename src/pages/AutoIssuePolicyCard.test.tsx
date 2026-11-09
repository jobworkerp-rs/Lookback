import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import type { AutoIssuePolicySnapshot, AutoIssueResolutionMode } from "@/types/api";
import { AutoIssuePolicyCard } from "./AutoIssuePolicyCard";

const getAutoIssuePolicy = vi.fn();
const setAutoIssuePolicy = vi.fn();

vi.mock("@/api", () => ({
  getAutoIssuePolicy: () => getAutoIssuePolicy(),
  setAutoIssuePolicy: (request: unknown) => setAutoIssuePolicy(request),
}));

const snapshot: AutoIssuePolicySnapshot = {
  version: 1,
  changed_at_ms: 0,
  policy: {
    extraction_enabled: true,
    auto_register_min_confidence: 0.9,
    auto_register_priority_range: { lower: "P0", upper: "P2" },
    explicit_only: true,
    allowed_kinds: ["TASK"],
    extract_cap: 5,
    resolution_mode: "DISABLED",
    resolution_min_confidence: 0.9,
  },
};

function renderCard() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <I18nextProvider i18n={i18n}>
      <QueryClientProvider client={client}>
        <AutoIssuePolicyCard />
      </QueryClientProvider>
    </I18nextProvider>,
  );
}

describe("AutoIssuePolicyCard", () => {
  beforeEach(async () => {
    await i18n.changeLanguage("en");
    vi.clearAllMocks();
    getAutoIssuePolicy.mockResolvedValue(snapshot);
    setAutoIssuePolicy.mockResolvedValue({ ...snapshot, version: 2 });
  });

  it("updates the persisted policy and refreshes the query after a successful save", async () => {
    renderCard();
    const extractionEnabled = await screen.findByRole("checkbox", {
      name: "Enable new Candidate extraction",
    });
    expect(extractionEnabled.parentElement).toHaveClass("settings-checkbox-row");
    expect(extractionEnabled.nextElementSibling).toHaveTextContent(
      "Enable new Candidate extraction",
    );
    const confidence = await screen.findByLabelText(
      "Minimum confidence for automatic registration",
    );
    fireEvent.change(confidence, { target: { value: "0.95" } });
    fireEvent.click(screen.getByRole("button", { name: "Save extraction policy" }));

    await waitFor(() =>
      expect(setAutoIssuePolicy).toHaveBeenCalledWith({
        policy: expect.objectContaining({ auto_register_min_confidence: 0.95 }),
      }),
    );
    await waitFor(() => expect(getAutoIssuePolicy).toHaveBeenCalledTimes(2));
  });

  it("places the extraction cap directly after the extraction toggle", async () => {
    renderCard();
    const extractionEnabled = await screen.findByRole("checkbox", {
      name: "Enable new Candidate extraction",
    });
    const extractCap = screen.getByLabelText("Candidates saved per extraction");

    expect(extractionEnabled.compareDocumentPosition(extractCap)).toBe(
      Node.DOCUMENT_POSITION_FOLLOWING,
    );
  });

  it("does not save an out-of-range confidence and explains the input error", async () => {
    renderCard();
    const confidence = await screen.findByLabelText(
      "Minimum confidence for automatic registration",
    );
    fireEvent.change(confidence, { target: { value: "1.1" } });
    fireEvent.click(screen.getByRole("button", { name: "Save extraction policy" }));

    expect(await screen.findByText("Confidence must be between 0 and 1.")).toBeInTheDocument();
    expect(setAutoIssuePolicy).not.toHaveBeenCalled();
  });

  it("saves the resolution mode shared by same-thread and cross-thread matching", async () => {
    renderCard();
    const mode = await screen.findByLabelText("Issue resolution mode");
    const confidence = screen.getByLabelText("Minimum confidence for Issue resolution");

    fireEvent.change(mode, { target: { value: "APPLY" satisfies AutoIssueResolutionMode } });
    fireEvent.change(confidence, { target: { value: "0.75" } });
    fireEvent.click(screen.getByRole("button", { name: "Save extraction policy" }));

    await waitFor(() =>
      expect(setAutoIssuePolicy).toHaveBeenCalledWith({
        policy: expect.objectContaining({
          resolution_mode: "APPLY",
          resolution_min_confidence: 0.75,
        }),
      }),
    );
  });

  it("does not save an invalid Issue resolution confidence", async () => {
    renderCard();
    const mode = await screen.findByLabelText("Issue resolution mode");
    fireEvent.change(mode, { target: { value: "APPLY" } });
    fireEvent.change(screen.getByLabelText("Minimum confidence for Issue resolution"), {
      target: { value: "1.1" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save extraction policy" }));

    expect(
      await screen.findByText("Resolution confidence must be between 0 and 1."),
    ).toBeInTheDocument();
    expect(setAutoIssuePolicy).not.toHaveBeenCalled();
  });

  it("disables automatic-registration conditions while extraction is disabled", async () => {
    renderCard();
    const extraction = await screen.findByRole("checkbox", {
      name: "Enable new Candidate extraction",
    });
    fireEvent.click(extraction);

    expect(screen.getByRole("group", { name: "Extraction conditions" })).toBeDisabled();
    expect(
      screen.getByRole("checkbox", { name: /Automatically register Candidates as Issues/ }),
    ).toBeDisabled();
    expect(screen.getByLabelText("Minimum confidence for automatic registration")).toBeDisabled();
    expect(screen.getByRole("checkbox", { name: "TASK" })).toBeDisabled();
    expect(
      screen.getByText(
        "Extraction is disabled, so new Candidates are not extracted or saved and these automatic-registration conditions do not apply.",
      ),
    ).toBeInTheDocument();
  });

  it("disables automatic-registration conditions and explains manual registration", async () => {
    renderCard();

    const autoRegister = await screen.findByRole("checkbox", {
      name: /Automatically register Candidates as Issues/,
    });
    expect(autoRegister).toBeChecked();
    fireEvent.click(autoRegister);

    expect(autoRegister).not.toBeChecked();
    expect(autoRegister.nextElementSibling).toHaveTextContent(
      "Candidates are saved, but are not automatically registered as Issues. Review them in Review items to register them as Issues.",
    );
    expect(screen.getByLabelText("Minimum confidence for automatic registration")).toBeDisabled();
    expect(screen.getByLabelText("Lowest priority")).toBeDisabled();
    expect(screen.getByLabelText("Highest priority")).toBeDisabled();
    expect(
      screen.getByRole("checkbox", { name: "Automatically register only explicit candidates" }),
    ).toBeDisabled();
    expect(
      screen.getByText(
        "Configure new Candidate extraction and the conditions for automatically registering Candidates as Issues.",
      ),
    ).toBeInTheDocument();
  });

  it("uses the Japanese manual-registration explanation when Japanese is selected", async () => {
    await i18n.changeLanguage("ja");
    renderCard();

    expect(
      await screen.findByText("下の自動登録条件に合う候補は、自動で Issue に登録されます。"),
    ).toBeInTheDocument();
  });

  it("shows a save failure without replacing the current policy", async () => {
    setAutoIssuePolicy.mockRejectedValueOnce(new Error("storage unavailable"));
    renderCard();
    await screen.findByLabelText("Enable new Candidate extraction");
    fireEvent.click(screen.getByRole("button", { name: "Save extraction policy" }));

    expect(await screen.findByText(/Could not update extraction policy/)).toBeInTheDocument();
  });
});
