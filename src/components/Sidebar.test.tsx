import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { Sidebar } from "./Sidebar";

const baseProps = {
  current: "threads" as const,
  threadCount: 3,
  summaryCount: 2,
  sidecar: {
    phase: "ready" as const,
    warnings: [],
    endpoints: {
      jobworkerp_port: 9000,
      memories_port: 9010,
      conductor_port: 9020,
      mcp_server_port: null,
    },
  },
  theme: { pref: "system" as const, setPref: vi.fn() },
  locale: { pref: "system" as const, resolved: "ja" as const, setPref: vi.fn() },
};

describe("Sidebar", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("routes to periodic tasks from the System section", () => {
    const onChange = vi.fn();
    renderWithProviders(<Sidebar {...baseProps} onChange={onChange} />);

    fireEvent.click(screen.getByText("定期実行"));
    expect(onChange).toHaveBeenCalledWith("periodic");
  });
});
