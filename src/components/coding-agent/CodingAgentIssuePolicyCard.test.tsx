import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { I18nextProvider } from "react-i18next";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { codingAgentGetSettings } from "@/api";
import i18n from "@/i18n";
import type { CodingAgentSettingsResponse } from "@/types/api";
import { CodingAgentIssuePolicyCard } from "./CodingAgentIssuePolicyCard";
import {
  CodingAgentSettingsEditorProvider,
  useCodingAgentSettingsEditor,
} from "./CodingAgentSettingsEditorContext";

vi.mock("@/api", () => ({
  codingAgentGetSettings: vi.fn(),
  codingAgentPrepareProfileHome: vi.fn(),
  codingAgentUpdateSettings: vi.fn(),
}));

const response: CodingAgentSettingsResponse = {
  settings: {
    formatVersion: 1,
    revision: 1,
    activeProfileId: null,
    semanticJudgeEnabled: true,
    semanticJudgePolicy: { maxRetries: 1, deadlineSeconds: 60 },
    autoDoneEnabled: true,
    profiles: [],
    repositoryHosts: [],
  },
  profileMountAvailability: [],
  availableAgentPresets: [],
};

function DirtyState() {
  const editor = useCodingAgentSettingsEditor();
  return (
    <output data-testid="state">{`${editor.dirty}:${editor.settings?.autoDoneEnabled}`}</output>
  );
}

function DisableJudgeButton() {
  const editor = useCodingAgentSettingsEditor();
  return (
    <button
      type="button"
      onClick={() =>
        editor.update((settings) => ({
          ...settings,
          semanticJudgeEnabled: false,
          autoDoneEnabled: false,
        }))
      }
    >
      disable judge
    </button>
  );
}

function renderCard(issueEnabled = true) {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={queryClient}>
      <I18nextProvider i18n={i18n}>
        <CodingAgentSettingsEditorProvider>
          <CodingAgentIssuePolicyCard issueEnabled={issueEnabled} />
          <DisableJudgeButton />
          <DirtyState />
        </CodingAgentSettingsEditorProvider>
      </I18nextProvider>
    </QueryClientProvider>,
  );
}

describe("CodingAgentIssuePolicyCard", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    vi.clearAllMocks();
    vi.mocked(codingAgentGetSettings).mockResolvedValue(response);
  });

  it("edits the shared draft and clears auto-completion when judging is disabled", async () => {
    renderCard();
    await screen.findByRole("checkbox", { name: "意味判定を実施する" });
    await waitFor(() => expect(screen.getByTestId("state")).toHaveTextContent("false:true"));
    const autoDone = screen.getByRole("checkbox", { name: "合格したIssueを自動完了する" });
    expect(autoDone).toBeChecked();
    fireEvent.click(screen.getByRole("button", { name: "disable judge" }));
    await waitFor(() => expect(screen.getByTestId("state")).toHaveTextContent("true:false"));
    expect(autoDone).not.toBeChecked();
    expect(autoDone).toBeDisabled();
  });

  it("keeps Issue policy editable while showing the disabled-feature notice", async () => {
    renderCard(false);
    await screen.findByRole("checkbox", { name: "意味判定を実施する" });
    expect(screen.getAllByRole("status")[0]).toHaveTextContent("Issue機能が無効");
    expect(screen.getByRole("checkbox", { name: "意味判定を実施する" })).toBeEnabled();
  });
});
