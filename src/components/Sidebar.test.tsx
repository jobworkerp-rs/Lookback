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
      issue_tracker_port: null,
    },
  },
  theme: { pref: "system" as const, setPref: vi.fn() },
  locale: { pref: "system" as const, resolved: "ja" as const, setPref: vi.fn() },
  experimentalIssuesEnabled: true,
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

  it("uses the active group count for the Threads badge when available", () => {
    renderWithProviders(<Sidebar {...baseProps} threadGroupCount={4} onChange={vi.fn()} />);

    const threadsButton = screen.getByRole("button", { name: /スレッド/ });
    expect(threadsButton).toHaveTextContent("4");
    expect(threadsButton).not.toHaveTextContent("3");
  });

  it("shows Issue Tracker as unavailable when startup reported its failure", () => {
    renderWithProviders(
      <Sidebar
        {...baseProps}
        onChange={vi.fn()}
        sidecar={{
          ...baseProps.sidecar,
          warnings: [
            {
              kind: "issue_tracker_unavailable",
              message: "health check timed out",
              detail: "health check timed out",
            },
          ],
        }}
      />,
    );

    expect(screen.getByText("issue-tracker")).toBeInTheDocument();
    expect(screen.getByText("利用不能")).toBeInTheDocument();
  });

  it("labels services without a local port as remote", () => {
    renderWithProviders(
      <Sidebar
        {...baseProps}
        onChange={vi.fn()}
        sidecar={{
          ...baseProps.sidecar,
          endpoints: {
            ...baseProps.sidecar.endpoints,
            jobworkerp_port: null,
            memories_port: null,
            conductor_port: 9020,
          },
        }}
      />,
    );

    expect(screen.getAllByText("remote")).toHaveLength(2);
    expect(screen.getByText(":9020")).toBeInTheDocument();
    expect(screen.queryByText(":null")).toBeNull();
  });

  it("shows the Issue Tracker port when it is ready", () => {
    renderWithProviders(
      <Sidebar
        {...baseProps}
        onChange={vi.fn()}
        sidecar={{
          ...baseProps.sidecar,
          endpoints: { ...baseProps.sidecar.endpoints, issue_tracker_port: 9030 },
        }}
      />,
    );

    expect(screen.getByText("issue-tracker")).toBeInTheDocument();
    expect(screen.getByText(":9030")).toBeInTheDocument();
  });

  it("hides all experimental Issues surfaces when the capability is disabled", () => {
    renderWithProviders(
      <Sidebar
        {...baseProps}
        onChange={vi.fn()}
        experimentalIssuesEnabled={false}
        sidecar={{
          ...baseProps.sidecar,
          warnings: [
            {
              kind: "issue_tracker_unavailable",
              message: "health check timed out",
              detail: "health check timed out",
            },
          ],
        }}
      />,
    );

    expect(screen.queryByRole("button", { name: "Issues" })).not.toBeInTheDocument();
    expect(screen.queryByText("issue-tracker")).not.toBeInTheDocument();
    expect(screen.queryByText("利用不能")).not.toBeInTheDocument();
    expect(screen.getByText("jobworkerp")).toBeInTheDocument();
  });
});
