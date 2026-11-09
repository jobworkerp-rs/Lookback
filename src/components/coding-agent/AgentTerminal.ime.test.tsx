import { fireEvent, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { renderWithProviders } from "@/test-utils";
import { AgentTerminal } from "./AgentTerminal";
import type { AgentTerminalBridge } from "./types";

// jsdom has no canvas, so a minimal stand-in exposes xterm's input textarea.
vi.mock("@xterm/xterm", () => {
  class Terminal {
    options = { disableStdin: false };
    private listeners = new Set<(data: string) => void>();
    open(container: HTMLElement) {
      const textarea = document.createElement("textarea");
      textarea.className = "xterm-helper-textarea";
      container.append(textarea);
    }
    write() {}
    loadAddon() {}
    onData(listener: (data: string) => void) {
      this.listeners.add(listener);
      return { dispose: () => this.listeners.delete(listener) };
    }
    onBinary() {
      return { dispose: () => {} };
    }
    dispose() {}
  }
  return { Terminal };
});
vi.mock("@xterm/addon-fit", () => ({ FitAddon: class {} }));

function renderDefaultTerminal(status: "running" | "finished" = "running") {
  const bridge: AgentTerminalBridge = {
    subscribeOutput: vi.fn(() => () => {}),
    sendInput: vi.fn(),
  };
  const result = renderWithProviders(
    <AgentTerminal
      bridge={bridge}
      session={{
        runId: "run-ime",
        jobId: "job-ime",
        purpose: "authentication",
        status,
        ownsSession: true,
        feedStatus: "active",
      }}
    />,
  );
  return { ...result, bridge };
}

async function helperTextarea(container: HTMLElement) {
  return waitFor(() => {
    const element = container.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
    if (!element) throw new Error("terminal textarea is not mounted yet");
    return element;
  });
}

function typeThroughIme(textarea: HTMLTextAreaElement, text: string) {
  // fcitx on WebKitGTK reports keyCode 229 and commits text asynchronously.
  for (const _ of text) fireEvent.keyDown(textarea, { keyCode: 229 });
  for (const character of text) {
    textarea.dispatchEvent(
      new InputEvent("input", { data: character, inputType: "insertText", bubbles: true }),
    );
  }
}

function typeThroughFcitx(textarea: HTMLTextAreaElement, text: string) {
  // Recorded from fcitx5 on WebKitGTK: one composition per key, no compositionstart.
  for (const character of text) {
    fireEvent.keyDown(textarea, { keyCode: 229 });
    textarea.value += character;
    textarea.dispatchEvent(
      new InputEvent("input", {
        data: character,
        inputType: "insertFromComposition",
        isComposing: true,
        bubbles: true,
      }),
    );
    textarea.dispatchEvent(
      new CompositionEvent("compositionend", { data: character, bubbles: true }),
    );
  }
}

function sentText(bridge: AgentTerminalBridge) {
  return vi
    .mocked(bridge.sendInput)
    .mock.calls.map(([, bytes]) => new TextDecoder().decode(bytes))
    .join("");
}

describe("AgentTerminal default renderer IME input", () => {
  it("sends IME-committed text to the session exactly once", async () => {
    const { container, bridge } = renderDefaultTerminal();
    typeThroughIme(await helperTextarea(container), "codex");

    expect(sentText(bridge)).toBe("codex");
  });

  it("sends fcitx composition commits once, including Japanese kana", async () => {
    const { container, bridge } = renderDefaultTerminal();
    const textarea = await helperTextarea(container);
    typeThroughFcitx(textarea, "codex");
    typeThroughFcitx(textarea, "あかさたな");

    expect(sentText(bridge)).toBe("codexあかさたな");
  });

  it("does not send IME-committed text when the session cannot accept input", async () => {
    const { container, bridge } = renderDefaultTerminal("finished");
    typeThroughIme(await helperTextarea(container), "codex");

    expect(bridge.sendInput).not.toHaveBeenCalled();
  });

  it("stops forwarding IME input after the terminal unmounts", async () => {
    const { container, bridge, unmount } = renderDefaultTerminal();
    const textarea = await helperTextarea(container);
    const host = textarea.parentElement as HTMLElement;
    unmount();
    document.body.append(host);
    typeThroughIme(textarea, "codex");

    expect(bridge.sendInput).not.toHaveBeenCalled();
  });
});
