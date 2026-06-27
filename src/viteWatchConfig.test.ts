import { describe, expect, it } from "vitest";
import { VITE_WATCH_IGNORED } from "./lib/viteWatchIgnored";

describe("vite config", () => {
  it("ignores Rust build output while watching files", () => {
    expect(VITE_WATCH_IGNORED).toEqual(expect.arrayContaining(["**/src-tauri/**", "**/target/**"]));
  });
});
