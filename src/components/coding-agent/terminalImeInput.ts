/** keyCode reported for every key an input method (IME) intercepts. */
const IME_PROCESS_KEY = 229;

/** Edits an input method may report instead of a plain key, as terminal bytes. */
const CONTROL_INPUT: Record<string, string> = {
  deleteContentBackward: "\x7f",
  insertLineBreak: "\r",
};

const COMPOSITION_EVENTS = ["compositionstart", "compositionupdate", "compositionend"] as const;

/**
 * Send text committed by an input method (IME) to the terminal exactly once.
 *
 * xterm.js reconstructs IME input from its hidden textarea: a composition
 * sends the textarea slice starting where `compositionstart` was seen, and a
 * keyCode-229 key sends a value diff taken after a timeout. fcitx5 on
 * WebKitGTK commits every key as a one-character composition without
 * `compositionstart`, and the textarea is never cleared, so xterm resent the
 * whole accumulated textarea on each key (`c`, `co`, `cod`, ...) and the shell
 * received that text. The events already carry the committed text, so it is
 * sent as-is, the textarea is cleared, and these events are kept away from
 * xterm. Keys that do not go through an input method stay with xterm.
 *
 * Listeners run in the capture phase on the terminal container so they fire
 * before xterm's own textarea listeners.
 */
export function installImeCommitBridge(
  container: HTMLElement,
  emit: (data: string) => void,
  inputEnabled: () => boolean,
): () => void {
  let lastKeyRoutedThroughIme = false;

  const commit = (event: Event, data: string | null | undefined) => {
    event.stopPropagation();
    if (event.target instanceof HTMLTextAreaElement) event.target.value = "";
    if (data && inputEnabled()) emit(data);
  };
  const onKeyDown = (event: KeyboardEvent) => {
    lastKeyRoutedThroughIme = event.keyCode === IME_PROCESS_KEY && !event.isComposing;
    if (lastKeyRoutedThroughIme) event.stopPropagation();
  };
  // Some input methods (IBus) commit through a plain insertText input instead.
  const onInput = (event: Event) => {
    if (!lastKeyRoutedThroughIme || !(event instanceof InputEvent) || event.isComposing) return;
    const data = event.inputType === "insertText" ? event.data : CONTROL_INPUT[event.inputType];
    if (data === undefined) return;
    commit(event, data);
  };
  const onComposition = (event: Event) => {
    if (event.type === "compositionend") {
      commit(event, (event as CompositionEvent).data);
    } else {
      event.stopPropagation();
    }
  };

  container.addEventListener("keydown", onKeyDown, true);
  container.addEventListener("input", onInput, true);
  for (const type of COMPOSITION_EVENTS) container.addEventListener(type, onComposition, true);
  return () => {
    container.removeEventListener("keydown", onKeyDown, true);
    container.removeEventListener("input", onInput, true);
    for (const type of COMPOSITION_EVENTS) {
      container.removeEventListener(type, onComposition, true);
    }
  };
}
