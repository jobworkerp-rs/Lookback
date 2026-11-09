import { beforeEach, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import {
  hasWriteRejections,
  rejectionReasonLabel,
  splitWriteRejections,
} from "./embeddingWriteRejections";

describe("splitWriteRejections", () => {
  it("separates stale-result discards from rejections that need attention", () => {
    expect(
      splitWriteRejections({
        source_version_mismatch: 3,
        attempt_mismatch: 1,
        space_mismatch: 2,
        dimension_mismatch: 4,
        missing_token: 5,
      }),
    ).toEqual({
      advisories: [
        ["attempt_mismatch", 1],
        ["source_version_mismatch", 3],
      ],
      warnings: [
        ["dimension_mismatch", 4],
        ["missing_token", 5],
        ["space_mismatch", 2],
      ],
    });
  });

  it("drops reasons with a zero count", () => {
    expect(splitWriteRejections({ source_version_mismatch: 0, space_mismatch: 0 })).toEqual({
      advisories: [],
      warnings: [],
    });
  });

  it("treats an unknown reason as a warning so new server reasons are not hidden", () => {
    expect(splitWriteRejections({ entity_missing: 1 })).toEqual({
      advisories: [],
      warnings: [["entity_missing", 1]],
    });
  });

  it("returns empty lists for a missing or empty record", () => {
    expect(splitWriteRejections(undefined)).toEqual({ advisories: [], warnings: [] });
    expect(splitWriteRejections({})).toEqual({ advisories: [], warnings: [] });
  });
});

describe("hasWriteRejections", () => {
  it("is true when either list has an entry and false when both are empty", () => {
    expect(hasWriteRejections(splitWriteRejections({ source_version_mismatch: 1 }))).toBe(true);
    expect(hasWriteRejections(splitWriteRejections({ space_mismatch: 1 }))).toBe(true);
    expect(hasWriteRejections(splitWriteRejections({ space_mismatch: 0 }))).toBe(false);
  });
});

describe("rejectionReasonLabel", () => {
  beforeEach(() => {
    i18n.changeLanguage("ja");
  });

  it("returns a localized label for a known reason", () => {
    expect(rejectionReasonLabel(i18n.t, "source_version_mismatch")).toBe(
      "元データの更新後に届いた古い結果",
    );
  });

  it("falls back to a generic label that keeps the raw reason for an unknown reason", () => {
    expect(rejectionReasonLabel(i18n.t, "entity_missing")).toBe("不明な理由 (entity_missing)");
  });

  it("follows the selected language", async () => {
    await i18n.changeLanguage("en");
    expect(rejectionReasonLabel(i18n.t, "space_mismatch")).toBe(
      "Results from a different embedding space (model or dimension)",
    );
  });
});
