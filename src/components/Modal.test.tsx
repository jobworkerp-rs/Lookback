import { fireEvent, render, screen } from "@testing-library/react";
import { useRef, useState } from "react";
import { expect, it } from "vitest";
import { Modal } from "./Modal";

function NestedModals() {
  const [parentOpen, setParentOpen] = useState(true);
  const [childOpen, setChildOpen] = useState(true);
  if (!parentOpen) return null;

  return (
    <Modal ariaLabel="parent" onClose={() => setParentOpen(false)}>
      <span>parent content</span>
      {childOpen && (
        <Modal ariaLabel="child" onClose={() => setChildOpen(false)}>
          <span>child content</span>
        </Modal>
      )}
    </Modal>
  );
}

it("closes only the frontmost nested modal on Escape", () => {
  render(<NestedModals />);

  fireEvent.keyDown(window, { key: "Escape" });

  expect(screen.getByRole("dialog", { name: "parent" })).toBeInTheDocument();
  expect(screen.queryByRole("dialog", { name: "child" })).not.toBeInTheDocument();

  fireEvent.keyDown(window, { key: "Escape" });

  expect(screen.queryByRole("dialog", { name: "parent" })).not.toBeInTheDocument();
});

it("manages focus and restores the opener when opted in", () => {
  const opener = document.createElement("button");
  opener.textContent = "open";
  document.body.append(opener);
  opener.focus();

  const { unmount } = render(
    <Modal ariaLabel="managed" focusManagement onClose={() => {}}>
      <button type="button">first</button>
      <button type="button">second</button>
    </Modal>,
  );

  const first = screen.getByRole("button", { name: "first" });
  const second = screen.getByRole("button", { name: "second" });
  expect(first).toHaveFocus();

  second.focus();
  fireEvent.keyDown(second, { key: "Tab" });
  expect(first).toHaveFocus();
  fireEvent.keyDown(first, { key: "Tab", shiftKey: true });
  expect(second).toHaveFocus();

  unmount();
  expect(opener).toHaveFocus();
  opener.remove();
});

it("keeps legacy focus behavior unless focus management is enabled", () => {
  const opener = document.createElement("button");
  document.body.append(opener);
  opener.focus();

  const { unmount } = render(
    <Modal ariaLabel="legacy" onClose={() => {}}>
      <button type="button">dialog action</button>
    </Modal>,
  );

  expect(opener).toHaveFocus();
  unmount();
  opener.remove();
});

function SimultaneousManagedModals() {
  const [childOpen, setChildOpen] = useState(true);
  return (
    <Modal ariaLabel="managed parent" focusManagement onClose={() => {}}>
      <button type="button">parent action</button>
      {childOpen && (
        <Modal ariaLabel="managed child" focusManagement onClose={() => setChildOpen(false)}>
          <button type="button" onClick={() => setChildOpen(false)}>
            close child
          </button>
        </Modal>
      )}
    </Modal>
  );
}

function SequentialManagedModals() {
  const [childOpen, setChildOpen] = useState(false);
  return (
    <Modal ariaLabel="sequential parent" focusManagement onClose={() => {}}>
      <button type="button">parent first</button>
      <button type="button" onClick={() => setChildOpen(true)}>
        open child
      </button>
      {childOpen && (
        <Modal ariaLabel="sequential child" focusManagement onClose={() => setChildOpen(false)}>
          <button type="button" onClick={() => setChildOpen(false)}>
            close sequential child
          </button>
        </Modal>
      )}
    </Modal>
  );
}

it("returns focus to the parent when a simultaneously mounted child closes", () => {
  const opener = document.createElement("button");
  document.body.append(opener);
  opener.focus();

  const { unmount } = render(<SimultaneousManagedModals />);
  expect(screen.getByRole("button", { name: "close child" })).toHaveFocus();

  fireEvent.click(screen.getByRole("button", { name: "close child" }));
  expect(screen.getByRole("button", { name: "parent action" })).toHaveFocus();

  unmount();
  expect(opener).toHaveFocus();
  opener.remove();
});

it("restores the parent control that opened a child modal", () => {
  const opener = document.createElement("button");
  document.body.append(opener);
  opener.focus();

  const { unmount } = render(<SequentialManagedModals />);
  const childOpener = screen.getByRole("button", { name: "open child" });
  childOpener.focus();
  fireEvent.click(childOpener);
  expect(screen.getByRole("button", { name: "close sequential child" })).toHaveFocus();

  fireEvent.click(screen.getByRole("button", { name: "close sequential child" }));
  expect(childOpener).toHaveFocus();

  unmount();
  expect(opener).toHaveFocus();
  opener.remove();
});

it("traps focus around only tabbable controls", () => {
  const { unmount } = render(
    <Modal ariaLabel="filtered" focusManagement onClose={() => {}}>
      <button type="button">first tabbable</button>
      <button type="button" tabIndex={-1}>
        programmatic only
      </button>
      <fieldset disabled>
        <button type="button">disabled group</button>
      </fieldset>
      <button type="button">last tabbable</button>
      <div hidden>
        <button type="button">hidden control</button>
      </div>
    </Modal>,
  );

  const first = screen.getByRole("button", { name: "first tabbable" });
  const last = screen.getByRole("button", { name: "last tabbable" });
  expect(first).toHaveFocus();

  last.focus();
  const event = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
  last.dispatchEvent(event);
  expect(event.defaultPrevented).toBe(true);
  expect(first).toHaveFocus();

  unmount();
});

it("traps Shift+Tab when the initial focus target is not tabbable", () => {
  function HeadingTargetModal() {
    const headingRef = useRef<HTMLHeadingElement>(null);
    return (
      <Modal
        ariaLabel="heading target"
        focusManagement
        initialFocusRef={headingRef}
        onClose={() => {}}
      >
        <h2 ref={headingRef} tabIndex={-1}>
          dialog heading
        </h2>
        <button type="button">last action</button>
      </Modal>
    );
  }

  const { unmount } = render(<HeadingTargetModal />);
  const heading = screen.getByRole("heading", { name: "dialog heading" });
  const last = screen.getByRole("button", { name: "last action" });
  expect(heading).toHaveFocus();

  const event = new KeyboardEvent("keydown", {
    key: "Tab",
    shiftKey: true,
    bubbles: true,
    cancelable: true,
  });
  heading.dispatchEvent(event);
  expect(event.defaultPrevented).toBe(true);
  expect(last).toHaveFocus();
  unmount();
});
