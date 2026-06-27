import { fireEvent, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { ErrorBoundary } from "./ErrorBoundary";

function Boom(): never {
  throw new Error("kaboom");
}

describe("ErrorBoundary", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
    // React logs the caught error to console.error; silence the expected noise.
    vi.spyOn(console, "error").mockImplementation(() => {});
  });
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("renders a fallback with the error message instead of propagating the throw", () => {
    expect(() =>
      renderWithProviders(
        <ErrorBoundary>
          <Boom />
        </ErrorBoundary>,
      ),
    ).not.toThrow();
    expect(screen.getByText("表示中に問題が発生しました")).toBeInTheDocument();
    expect(screen.getByText("kaboom")).toBeInTheDocument();
  });

  it("clears the error and re-renders children when reset is clicked", () => {
    let shouldThrow = true;
    function Maybe() {
      if (shouldThrow) throw new Error("kaboom");
      return <div>ok</div>;
    }
    renderWithProviders(
      <ErrorBoundary>
        <Maybe />
      </ErrorBoundary>,
    );
    expect(screen.getByText("表示中に問題が発生しました")).toBeInTheDocument();
    shouldThrow = false;
    fireEvent.click(screen.getByText("再読み込み"));
    expect(screen.getByText("ok")).toBeInTheDocument();
  });

  it("renders children normally when nothing throws", () => {
    renderWithProviders(
      <ErrorBoundary>
        <div>healthy</div>
      </ErrorBoundary>,
    );
    expect(screen.getByText("healthy")).toBeInTheDocument();
  });
});
