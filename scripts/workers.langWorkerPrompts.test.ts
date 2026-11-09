import { describe, expect, it } from "vitest";
import groupSystemEn from "../workers/lang-workers/workers/thread-group-summary/prompts/system_prompt.en.txt?raw";
import groupSystemJa from "../workers/lang-workers/workers/thread-group-summary/prompts/system_prompt.ja.txt?raw";
import groupTailEn from "../workers/lang-workers/workers/thread-group-summary/prompts/user_tail.en.txt?raw";
import groupTailJa from "../workers/lang-workers/workers/thread-group-summary/prompts/user_tail.ja.txt?raw";
import groupSingle from "../workers/lang-workers/workers/thread-group-summary/thread-group-summary-single.yaml?raw";
import threadSystemEn from "../workers/lang-workers/workers/thread-summary/prompts/system_prompt.en.txt?raw";
import threadSystemJa from "../workers/lang-workers/workers/thread-summary/prompts/system_prompt.ja.txt?raw";

// These prompts are hand-merged from memories: agent-app's summary schema
// requires `has_unfinished_issue_candidates`, which the upstream prompts
// never mention. A wholesale prompt sync would silently drop it.
describe("hand-merged summary prompts", () => {
  it.each([
    ["thread-summary ja", threadSystemJa],
    ["thread-summary en", threadSystemEn],
    ["thread-group-summary ja", groupSystemJa],
    ["thread-group-summary en", groupSystemEn],
  ])("%s instructs the agent-app issue flag", (_name, prompt) => {
    expect(prompt).toContain("`has_unfinished_issue_candidates`");
    expect(prompt).not.toContain("`key_decisions`, and `status`.");
  });

  it("requires the issue flag in the thread-group schema the prompt describes", () => {
    expect(groupSingle).toMatch(/"required": \[[^\]]*"has_unfinished_issue_candidates"/);
  });

  it.each([
    ["system ja", groupSystemJa],
    ["system en", groupSystemEn],
    ["tail ja", groupTailJa],
    ["tail en", groupTailEn],
  ])("thread-group %s prompt is plain text baked into the worker", (_name, prompt) => {
    expect(prompt.trim()).not.toBe("");
    expect(prompt).not.toContain("{{");
    expect(prompt).not.toContain("{%");
  });
});
