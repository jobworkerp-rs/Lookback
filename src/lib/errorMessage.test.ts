import { describe, expect, it } from "vitest";
import { errorMessage } from "./errorMessage";

describe("errorMessage", () => {
  it("returns a thrown string verbatim (Tauri AppError serializes to a string)", () => {
    expect(errorMessage("gRPC error: not found")).toBe("gRPC error: not found");
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
