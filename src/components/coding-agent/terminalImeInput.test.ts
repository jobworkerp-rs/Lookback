import { afterEach, describe, expect, it } from "vitest";
import { installImeCommitBridge } from "./terminalImeInput";

function setup(enabled = true) {
  const container = document.createElement("div");
  const textarea = document.createElement("textarea");
  container.append(textarea);
  document.body.append(container);
  const emitted: string[] = [];
  const reachedTextarea: string[] = [];
  // Stands in for xterm.js, which listens on its own textarea.
  textarea.addEventListener("keydown", () => reachedTextarea.push("keydown"), true);
  textarea.addEventListener("input", () => reachedTextarea.push("input"), true);
  for (const type of ["compositionstart", "compositionupdate", "compositionend"]) {
    textarea.addEventListener(type, () => reachedTextarea.push(type), true);
  }
  const dispose = installImeCommitBridge(
    container,
    (data) => emitted.push(data),
    () => enabled,
  );
  const keydown = (keyCode: number, isComposing = false) =>
    textarea.dispatchEvent(
      new KeyboardEvent("keydown", { keyCode, isComposing, bubbles: true, cancelable: true }),
    );
  const commit = (data: string, isComposing = false) => {
    textarea.value += data;
    textarea.dispatchEvent(
      new InputEvent("input", { data, inputType: "insertText", isComposing, bubbles: true }),
    );
  };
  const composition = (type: string, data: string) =>
    textarea.dispatchEvent(new CompositionEvent(type, { data, bubbles: true }));
  // Event order recorded from fcitx5 on WebKitGTK: no compositionstart/update,
  // and the textarea keeps every committed character.
  const fcitxCommit = (data: string) => {
    keydown(229);
    textarea.value += data;
    textarea.dispatchEvent(
      new InputEvent("input", {
        data,
        inputType: "insertFromComposition",
        isComposing: true,
        bubbles: true,
      }),
    );
    composition("compositionend", data);
  };
  return {
    container,
    textarea,
    emitted,
    reachedTextarea,
    dispose,
    keydown,
    commit,
    composition,
    fcitxCommit,
  };
}

afterEach(() => {
  document.body.innerHTML = "";
});

describe("installImeCommitBridge", () => {
  it("sends each IME-committed character exactly once even when keydowns overtake commits", () => {
    const { emitted, keydown, commit, textarea } = setup();
    // fcitx commits asynchronously: several keydowns arrive before their text.
    for (const _ of "codex") keydown(229);
    for (const key of "codex") commit(key);

    expect(emitted.join("")).toBe("codex");
    expect(textarea.value).toBe("");
  });

  it("keeps IME-committed events away from xterm's textarea diffing", () => {
    const { reachedTextarea, keydown, commit } = setup();
    keydown(229);
    commit("c");

    expect(reachedTextarea).toEqual([]);
  });

  it("leaves active compositions and ordinary keys to xterm", () => {
    const { emitted, reachedTextarea, keydown, commit } = setup();
    keydown(229, true);
    commit("か", true);
    keydown(67);
    commit("c");

    expect(emitted).toEqual([]);
    expect(reachedTextarea).toEqual(["keydown", "input", "keydown", "input"]);
  });

  it("drops committed text while terminal input is disabled", () => {
    const { emitted, keydown, commit } = setup(false);
    keydown(229);
    commit("c");

    expect(emitted).toEqual([]);
  });

  it("stops intercepting after dispose", () => {
    const { emitted, reachedTextarea, keydown, commit, dispose } = setup();
    dispose();
    keydown(229);
    commit("c");

    expect(emitted).toEqual([]);
    expect(reachedTextarea).toEqual(["keydown", "input"]);
  });

  it("translates IME-routed backspace and enter into terminal control bytes", () => {
    const { emitted, keydown, textarea } = setup();
    for (const inputType of ["deleteContentBackward", "insertLineBreak"]) {
      keydown(229);
      textarea.dispatchEvent(new InputEvent("input", { inputType, bubbles: true }));
    }

    expect(emitted).toEqual(["\x7f", "\r"]);
  });

  it("leaves other IME-routed edits untouched", () => {
    const { emitted, reachedTextarea, keydown, textarea } = setup();
    keydown(229);
    textarea.dispatchEvent(
      new InputEvent("input", { inputType: "insertFromPaste", bubbles: true }),
    );

    expect(emitted).toEqual([]);
    expect(reachedTextarea).toEqual(["input"]);
  });

  it("sends each fcitx composition commit once instead of xterm's accumulated textarea", () => {
    const { emitted, reachedTextarea, textarea, fcitxCommit } = setup();
    for (const character of "codex") fcitxCommit(character);

    expect(emitted.join("")).toBe("codex");
    expect(textarea.value).toBe("");
    expect(reachedTextarea).not.toContain("compositionend");
  });

  it("sends Japanese kana committed one composition at a time", () => {
    const { emitted, fcitxCommit } = setup();
    for (const character of "あかさたな") fcitxCommit(character);

    expect(emitted.join("")).toBe("あかさたな");
  });

  it("sends only the final text of a multi-step conversion", () => {
    const { emitted, reachedTextarea, composition } = setup();
    composition("compositionstart", "");
    composition("compositionupdate", "かんじ");
    composition("compositionupdate", "漢字");
    composition("compositionend", "漢字");

    expect(emitted).toEqual(["漢字"]);
    expect(reachedTextarea).toEqual([]);
  });

  it("drops composition commits while terminal input is disabled", () => {
    const { emitted, fcitxCommit } = setup(false);
    fcitxCommit("c");

    expect(emitted).toEqual([]);
  });
});
