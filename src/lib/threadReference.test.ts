import { describe, expect, it } from "vitest";
import { parseThreadReference } from "./threadReference";

describe("parseThreadReference", () => {
  it("recognizes bare ids without losing snowflake precision", () => {
    expect(parseThreadReference("9007199254740993")).toEqual({
      kind: "both",
      id: "9007199254740993",
    });
  });

  it("recognizes explicit thread and memory prefixes", () => {
    expect(parseThreadReference("thread:42")).toEqual({ kind: "thread", id: "42" });
    expect(parseThreadReference(" MEMORY:42 ")).toEqual({ kind: "memory", id: "42" });
  });

  it("leaves non-exact ids to the normal browse filter", () => {
    expect(parseThreadReference("12 34")).toBeNull();
    expect(parseThreadReference("-42")).toBeNull();
    expect(parseThreadReference("issue 42")).toBeNull();
  });
});
