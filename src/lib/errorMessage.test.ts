import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { errorMessage } from "./errorMessage";

describe("errorMessage", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("translates a backend msg_id using the Japanese locale", () => {
    expect(errorMessage("periodic.validation.nameRequired")).toBe("名前を入力してください");
  });

  it("translates a backend msg_id using the English locale", () => {
    i18n.changeLanguage("en");
    expect(errorMessage("periodic.validation.sourceMismatch")).toBe(
      "source does not match sources",
    );
  });

  it("passes through a locale subtree key without exposing its object", () => {
    vi.spyOn(i18n, "t").mockReturnValue({ nameRequired: "名前を入力してください" } as never);
    expect(errorMessage("periodic.validation")).toBe("periodic.validation");
    vi.restoreAllMocks();
  });

  it("passes through an unknown dotted key", () => {
    expect(errorMessage("periodic.validation.unknown")).toBe("periodic.validation.unknown");
  });

  it("returns a thrown string verbatim (Tauri AppError serializes to a string)", () => {
    expect(errorMessage("gRPC error: not found")).toBe("gRPC error: not found");
  });

  it("leaves a Japanese legacy free-form message untouched", () => {
    expect(errorMessage("対象 source を選択してください")).toBe("対象 source を選択してください");
  });

  it("extracts .message from an Error instance", () => {
    expect(errorMessage(new Error("boom"))).toBe("boom");
  });

  it("stringifies non-string, non-Error values so a reason always shows", () => {
    expect(errorMessage(42)).toBe("42");
    expect(errorMessage(null)).toBe("null");
    expect(errorMessage(undefined)).toBe("undefined");
  });
});
