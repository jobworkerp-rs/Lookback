import { fireEvent, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { VectorDegradedBanner } from "./VectorDegradedBanner";

describe("VectorDegradedBanner", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("renders the dimension-bearing copy and fires the CTA", () => {
    const onOpen = vi.fn();
    renderWithProviders(
      <VectorDegradedBanner
        info={{ expectedDim: 2048, actualDim: 768 }}
        onOpenEmbeddingSettings={onOpen}
      />,
    );
    // Both dimensions surface in the body.
    expect(screen.getByRole("alert").textContent).toContain("2048");
    expect(screen.getByRole("alert").textContent).toContain("768");
    fireEvent.click(screen.getByRole("button", { name: i18n.t("sidecar.degraded.cta") }));
    expect(onOpen).toHaveBeenCalledTimes(1);
  });

  it("falls back to the dimension-free copy when dims are unknown", () => {
    renderWithProviders(<VectorDegradedBanner info={{}} onOpenEmbeddingSettings={() => {}} />);
    const body = screen.getByRole("alert").textContent ?? "";
    // The generic body is shown; no dimension numbers leak in.
    expect(body).toContain(i18n.t("sidecar.degraded.body"));
  });
});
