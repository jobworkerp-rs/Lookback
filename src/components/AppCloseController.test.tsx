import type { EventCallback } from "@tauri-apps/api/event";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  acknowledgeCloseChoiceRequest,
  chooseCloseAction,
  registerCloseChoiceUi,
  syncCloseChoiceLocale,
} from "@/api";
import i18n from "@/i18n";
import { renderWithProviders } from "@/test-utils";
import { AppCloseController } from "./AppCloseController";

const { listeners, listenMock, quitAppMock, unlistenCloseMock } = vi.hoisted(() => ({
  listeners: new Map<string, (event: { payload: unknown }) => void>(),
  listenMock: vi.fn(),
  quitAppMock: vi.fn(),
  unlistenCloseMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: listenMock,
}));

vi.mock("@/api", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api")>();
  return {
    ...actual,
    acknowledgeCloseChoiceRequest: vi.fn(),
    chooseCloseAction: vi.fn(),
    quitApp: quitAppMock,
    registerCloseChoiceUi: vi.fn(),
    syncCloseChoiceLocale: vi.fn(),
  };
});

function emit(event: string, payload: unknown = {}) {
  listeners.get(event)?.({ payload });
}

function deferred<T>() {
  let resolve: (value: T) => void = () => {};
  let reject: (reason?: unknown) => void = () => {};
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

describe("AppCloseController", () => {
  beforeEach(() => {
    listeners.clear();
    unlistenCloseMock.mockReset();
    listenMock
      .mockReset()
      .mockImplementation(async (event: string, callback: EventCallback<unknown>) => {
        listeners.set(event, callback as (event: { payload: unknown }) => void);
        return () => {
          listeners.delete(event);
          if (event === "window-lifecycle://close-requested") unlistenCloseMock();
        };
      });
    vi.mocked(acknowledgeCloseChoiceRequest).mockReset().mockResolvedValue(true);
    vi.mocked(chooseCloseAction).mockReset().mockResolvedValue("accepted");
    vi.mocked(registerCloseChoiceUi).mockReset().mockResolvedValue({ pendingRequest: null });
    vi.mocked(syncCloseChoiceLocale).mockReset().mockResolvedValue(undefined);
    quitAppMock.mockReset().mockResolvedValue(undefined);
    void i18n.changeLanguage("ja");
  });

  it("registers using the UI locale, acknowledges close events, and exposes localized choices", async () => {
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalledWith("ja"));
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 17,
        backgroundAvailable: true,
      });
    });

    expect(acknowledgeCloseChoiceRequest).toHaveBeenCalledWith(17);
    expect(
      await screen.findByRole("dialog", { name: "Lookback を終了しますか？" }),
    ).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "キャンセル" }));
    await waitFor(() => expect(chooseCloseAction).toHaveBeenCalledWith(17, "cancel"));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());

    await act(async () => {
      await i18n.changeLanguage("en");
    });
    await waitFor(() => expect(syncCloseChoiceLocale).toHaveBeenLastCalledWith("en"));
  });

  it("ignores a request whose acknowledgement lost to the native fallback", async () => {
    vi.mocked(acknowledgeCloseChoiceRequest).mockResolvedValue(false);
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 4,
        backgroundAvailable: true,
      });
    });

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("hides background mode when the tray cannot restore the window", async () => {
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 19,
        backgroundAvailable: false,
        backgroundUnavailableReason: "unverified",
      });
    });

    expect(await screen.findByText("アプリを終了しますか？")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "バックグラウンドで実行" }),
    ).not.toBeInTheDocument();
    expect(chooseCloseAction).not.toHaveBeenCalled();
  });

  it("does not open a duplicate web dialog when the native retry fallback is active", async () => {
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://quit-failed", { nativeFallback: true });
    });

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("keeps the dialog retryable when shutdown fails and uses the translated error", async () => {
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 28,
        backgroundAvailable: true,
      });
    });
    await screen.findByRole("dialog");
    fireEvent.click(screen.getByRole("button", { name: "アプリを終了" }));
    await waitFor(() => expect(chooseCloseAction).toHaveBeenCalledWith(28, "quit"));

    await act(async () => {
      emit("window-lifecycle://quit-failed");
    });

    expect(
      await screen.findByText("アプリを安全に終了できませんでした。もう一度お試しください。"),
    ).toBeInTheDocument();
    const retry = screen.getByRole("button", { name: "アプリを終了" });
    expect(retry).toBeEnabled();
    fireEvent.click(retry);
    await waitFor(() => expect(quitAppMock).toHaveBeenCalledTimes(1));
  });

  it("does not let a delayed response to an old request clear a newer close choice", async () => {
    const oldAction = deferred<Awaited<ReturnType<typeof chooseCloseAction>>>();
    vi.mocked(chooseCloseAction).mockImplementation((requestId, _choice) => {
      return requestId === 31 ? oldAction.promise : Promise.resolve("accepted");
    });
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 31,
        backgroundAvailable: true,
      });
    });
    await screen.findByRole("dialog");
    fireEvent.click(screen.getByRole("button", { name: "キャンセル" }));
    await waitFor(() => expect(chooseCloseAction).toHaveBeenCalledWith(31, "cancel"));

    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 32,
        backgroundAvailable: true,
      });
    });
    await waitFor(() => expect(acknowledgeCloseChoiceRequest).toHaveBeenCalledWith(32));
    const cancel = await screen.findByRole("button", { name: "キャンセル" });
    expect(cancel).toBeEnabled();

    await act(async () => {
      oldAction.resolve("accepted");
      await oldAction.promise;
    });

    expect(await screen.findByRole("dialog")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "キャンセル" })).toBeEnabled();
    fireEvent.click(screen.getByRole("button", { name: "キャンセル" }));
    await waitFor(() => expect(chooseCloseAction).toHaveBeenCalledWith(32, "cancel"));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  });

  it("ignores a delayed error from an old request after a newer request is accepted", async () => {
    const oldAction = deferred<Awaited<ReturnType<typeof chooseCloseAction>>>();
    vi.mocked(chooseCloseAction).mockImplementation((requestId) => {
      return requestId === 41 ? oldAction.promise : Promise.resolve("accepted");
    });
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 41,
        backgroundAvailable: true,
      });
    });
    await screen.findByRole("dialog");
    fireEvent.click(screen.getByRole("button", { name: "バックグラウンドで実行" }));
    await waitFor(() => expect(chooseCloseAction).toHaveBeenCalledWith(41, "background"));

    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 42,
        backgroundAvailable: true,
      });
    });
    await waitFor(() => expect(acknowledgeCloseChoiceRequest).toHaveBeenCalledWith(42));
    expect(await screen.findByRole("dialog")).toBeInTheDocument();

    await act(async () => {
      oldAction.reject(new Error("stale background failure"));
      await oldAction.promise.catch(() => undefined);
    });

    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "キャンセル" })).toBeEnabled();
  });

  it("does not let a late acknowledgement replace a newer close request", async () => {
    const oldAcknowledgement = deferred<boolean>();
    vi.mocked(acknowledgeCloseChoiceRequest).mockImplementation((requestId) =>
      requestId === 61 ? oldAcknowledgement.promise : Promise.resolve(true),
    );
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 61,
        backgroundAvailable: true,
      });
    });
    await waitFor(() => expect(acknowledgeCloseChoiceRequest).toHaveBeenCalledWith(61));

    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 62,
        backgroundAvailable: true,
      });
    });
    await waitFor(() => expect(acknowledgeCloseChoiceRequest).toHaveBeenCalledWith(62));
    expect(await screen.findByRole("dialog")).toBeInTheDocument();

    await act(async () => {
      oldAcknowledgement.resolve(true);
      await oldAcknowledgement.promise;
    });
    fireEvent.click(screen.getByRole("button", { name: "キャンセル" }));

    await waitFor(() => expect(chooseCloseAction).toHaveBeenCalledWith(62, "cancel"));
    expect(chooseCloseAction).not.toHaveBeenCalledWith(61, "cancel");
  });

  it("keeps the quit retry after failure invalidates an in-flight close acknowledgement", async () => {
    const delayedAcknowledgement = deferred<boolean>();
    vi.mocked(acknowledgeCloseChoiceRequest).mockImplementation((requestId) =>
      requestId === 71 ? delayedAcknowledgement.promise : Promise.resolve(true),
    );
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 71,
        backgroundAvailable: true,
      });
    });
    await waitFor(() => expect(acknowledgeCloseChoiceRequest).toHaveBeenCalledWith(71));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();

    await act(async () => {
      emit("window-lifecycle://quit-failed", { nativeFallback: false });
    });
    expect(
      await screen.findByText("アプリを安全に終了できませんでした。もう一度お試しください。"),
    ).toBeInTheDocument();

    await act(async () => {
      delayedAcknowledgement.resolve(true);
      await delayedAcknowledgement.promise;
    });

    expect(
      await screen.findByText("アプリを安全に終了できませんでした。もう一度お試しください。"),
    ).toBeInTheDocument();
    const retry = screen.getByRole("button", { name: "アプリを終了" });
    expect(retry).toBeEnabled();
    fireEvent.click(retry);
    await waitFor(() => expect(quitAppMock).toHaveBeenCalledTimes(1));
    expect(chooseCloseAction).not.toHaveBeenCalled();
  });

  it("accepts a new close request after failure while ignoring the invalidated acknowledgement", async () => {
    const delayedAcknowledgement = deferred<boolean>();
    vi.mocked(acknowledgeCloseChoiceRequest).mockImplementation((requestId) =>
      requestId === 73 ? delayedAcknowledgement.promise : Promise.resolve(true),
    );
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 73,
        backgroundAvailable: true,
      });
    });
    await waitFor(() => expect(acknowledgeCloseChoiceRequest).toHaveBeenCalledWith(73));
    await act(async () => {
      emit("window-lifecycle://quit-failed", { nativeFallback: false });
    });
    await screen.findByText("アプリを安全に終了できませんでした。もう一度お試しください。");

    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 74,
        backgroundAvailable: true,
      });
    });
    await waitFor(() => expect(acknowledgeCloseChoiceRequest).toHaveBeenCalledWith(74));
    expect(await screen.findByRole("dialog")).toBeInTheDocument();

    await act(async () => {
      delayedAcknowledgement.resolve(true);
      await delayedAcknowledgement.promise;
    });
    fireEvent.click(screen.getByRole("button", { name: "キャンセル" }));

    await waitFor(() => expect(chooseCloseAction).toHaveBeenCalledWith(74, "cancel"));
    expect(chooseCloseAction).not.toHaveBeenCalledWith(73, "cancel");
  });

  it("drops queued requests and stale bootstrap data from before quit failure", async () => {
    const delayedBootstrap = deferred<{
      pendingRequest: {
        requestId: number;
        windowLabel: string;
        backgroundAvailable: boolean;
        backgroundUnavailableReason: null;
      } | null;
    }>();
    vi.mocked(registerCloseChoiceUi).mockReturnValue(delayedBootstrap.promise);
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 72,
        backgroundAvailable: true,
      });
      emit("window-lifecycle://quit-failed", { nativeFallback: false });
    });
    expect(
      await screen.findByText("アプリを安全に終了できませんでした。もう一度お試しください。"),
    ).toBeInTheDocument();

    await act(async () => {
      delayedBootstrap.resolve({
        pendingRequest: {
          requestId: 72,
          windowLabel: "main",
          backgroundAvailable: true,
          backgroundUnavailableReason: null,
        },
      });
      await delayedBootstrap.promise;
    });

    expect(acknowledgeCloseChoiceRequest).not.toHaveBeenCalled();
    expect(
      screen.getByText("アプリを安全に終了できませんでした。もう一度お試しください。"),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "アプリを終了" })).toBeEnabled();
  });

  it("does not acknowledge a close request until both lifecycle listeners are ready", async () => {
    const quitFailedListener = deferred<void>();
    listenMock.mockImplementation(async (event: string, callback: EventCallback<unknown>) => {
      if (event === "window-lifecycle://quit-failed") {
        await quitFailedListener.promise;
      }
      listeners.set(event, callback as (event: { payload: unknown }) => void);
      return () => listeners.delete(event);
    });
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(listenMock).toHaveBeenCalledTimes(2));
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 51,
        backgroundAvailable: true,
      });
    });
    expect(acknowledgeCloseChoiceRequest).not.toHaveBeenCalled();
    expect(registerCloseChoiceUi).not.toHaveBeenCalled();

    await act(async () => {
      quitFailedListener.resolve();
    });
    await waitFor(() => expect(acknowledgeCloseChoiceRequest).toHaveBeenCalledWith(51));
    expect(await screen.findByRole("dialog")).toBeInTheDocument();
  });

  it("unlistens an already-installed event listener when the other listener fails", async () => {
    const quitFailedListener = deferred<void>();
    listenMock.mockImplementation(async (event: string, callback: EventCallback<unknown>) => {
      if (event === "window-lifecycle://quit-failed") {
        await quitFailedListener.promise;
      }
      listeners.set(event, callback as (event: { payload: unknown }) => void);
      return () => {
        listeners.delete(event);
        if (event === "window-lifecycle://close-requested") unlistenCloseMock();
      };
    });
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(listenMock).toHaveBeenCalledTimes(2));
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 52,
        backgroundAvailable: true,
      });
      quitFailedListener.reject(new Error("quit-failed listener failed"));
    });

    await waitFor(() => expect(unlistenCloseMock).toHaveBeenCalledTimes(1));
    expect(listeners.has("window-lifecycle://close-requested")).toBe(false);
    expect(registerCloseChoiceUi).not.toHaveBeenCalled();
    expect(acknowledgeCloseChoiceRequest).not.toHaveBeenCalled();
  });

  it("does not acknowledge a retained callback after its UI registration is disposed", async () => {
    const view = renderWithProviders(<AppCloseController />);
    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    const closeCallback = listeners.get("window-lifecycle://close-requested");
    expect(closeCallback).toBeDefined();
    view.unmount();

    await act(async () => {
      closeCallback?.({
        payload: { requestId: 53, backgroundAvailable: true },
      });
    });

    expect(acknowledgeCloseChoiceRequest).not.toHaveBeenCalled();
  });

  it("retranslates a saved background failure when the UI language changes", async () => {
    vi.mocked(chooseCloseAction).mockResolvedValue("hideFailed");
    renderWithProviders(<AppCloseController />);

    await waitFor(() => expect(registerCloseChoiceUi).toHaveBeenCalled());
    await act(async () => {
      emit("window-lifecycle://close-requested", {
        requestId: 54,
        backgroundAvailable: true,
      });
    });
    await screen.findByRole("dialog");
    fireEvent.click(screen.getByRole("button", { name: "バックグラウンドで実行" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "ウィンドウを隠せませんでした。表示したままにしています。",
    );

    await act(async () => {
      await i18n.changeLanguage("en");
    });

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "The window could not be hidden and will remain visible.",
    );
  });
});
