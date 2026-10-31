import { ArrowDown, ArrowLeft, ArrowRight, ArrowUp, Keyboard, Settings, createIcons } from "lucide";

import { recordPageView } from "./analytics";
import type { LibraryApp, PlayerSession, Wie } from "./backend";
import { configStore } from "./config_store";
import type { SettingsController } from "./settings";

const KEY_MAP: Record<string, string> = {
  Digit1: "1",
  Digit2: "2",
  Digit3: "3",
  KeyQ: "4",
  KeyW: "5",
  KeyE: "6",
  KeyA: "7",
  KeyS: "8",
  KeyD: "9",
  KeyZ: "*",
  KeyX: "0",
  KeyC: "#",
  Backspace: "CLR",
  ArrowUp: "UP",
  ArrowLeft: "LEFT",
  ArrowRight: "RIGHT",
  ArrowDown: "DOWN",
  Enter: "OK",
  NumpadEnter: "OK",
  Space: "OK",
};
// Fraction of the d-pad radius around its center where no direction is pressed.
const DPAD_DEAD_ZONE = 0.2;
const TOOLBAR_HIDE_DELAY_MS = 3000;
const icons = {
  ArrowDown,
  ArrowLeft,
  ArrowRight,
  ArrowUp,
  Keyboard,
  Settings,
};

export const runApp = (backend: Wie, app: LibraryApp, settings: SettingsController): Promise<boolean> => new Promise((resolve, reject) => {
  const playerView = document.getElementById("player-view") as HTMLElement;
  const playerToolbar = playerView.querySelector(".player-toolbar") as HTMLElement;
  const playerOutput = document.getElementById("player-output") as HTMLElement;
  const playerTitle = document.getElementById("player-title") as HTMLElement;
  const playerStatus = document.getElementById("player-status") as HTMLElement;
  const playerWarning = document.getElementById("player-warning") as HTMLElement;
  const backToLibrary = document.getElementById("back-to-library") as HTMLButtonElement;
  const appSettings = document.getElementById("app-settings") as HTMLButtonElement;
  const keypadToggle = document.getElementById("keypad-toggle") as HTMLButtonElement;
  const dpad = document.getElementById("dpad") as HTMLElement;
  const buttons = playerView.querySelectorAll<HTMLButtonElement>("button[data-key]");

  const abortController = new AbortController();
  const inputs = new Map<string, string>();
  let session: PlayerSession | undefined;
  let preparing = true;
  let suspended = false;
  let ending = false;
  let start: Promise<PlayerSession | undefined>;
  let warningTimer: ReturnType<typeof setTimeout> | undefined;
  let dpadPointer: number | undefined;
  // Android games take input directly on the game screen instead of the virtual keypad.
  const touchInput = app.filename.toLowerCase().endsWith(".apk");
  let touchPointer: number | undefined;
  let touchPoint: [number, number] | undefined;
  let toolbarTimer: ReturnType<typeof setTimeout> | undefined;

  playerTitle.textContent = app.title;
  playerWarning.hidden = true;
  createIcons({ icons, root: playerView });

  const updateControls = () => {
    const busy = preparing || suspended || !session || ending;
    playerStatus.hidden = !preparing || ending;
    playerView.setAttribute("aria-busy", String(busy));
    for (const button of buttons) button.disabled = busy;
  };
  const finish = async (error?: unknown, failed = false) => {
    if (ending) return;
    ending = true;
    clearTimeout(warningTimer);
    clearTimeout(toolbarTimer);
    playerView.classList.remove("toolbar-hidden");
    abortController.abort();
    inputs.clear();
    updateControls();
    try {
      await (await start)?.stop();
      if (error !== undefined) reject(error);
      else resolve(failed);
    } catch (stopError) {
      if (failed) resolve(true);
      else reject(error ?? stopError);
    } finally {
      playerStatus.hidden = true;
      playerWarning.hidden = true;
      playerView.removeAttribute("aria-busy");
    }
  };

  const setKey = (source: string, key?: string) => {
    if (!session || preparing || suspended || ending) return;
    const previous = inputs.get(source);
    if (previous === key) return;
    if (previous) {
      inputs.delete(source);
      if (![...inputs.values()].includes(previous)) void session.key(previous, false).catch(finish);
    }
    if (key) {
      const pressed = [...inputs.values()].includes(key);
      inputs.set(source, key);
      if (!pressed) void session.key(key, true).catch(finish);
    }
  };
  const resetDpad = () => {
    dpadPointer = undefined;
    delete dpad.dataset.pressed;
  };
  const releaseKeys = () => {
    inputs.clear();
    resetDpad();
    touchPointer = undefined;
    touchPoint = undefined;
    if (session && !ending) void session.releaseKeys().catch(finish);
  };
  const showClassicKeypad = (classic: boolean) => {
    playerView.classList.toggle("classic-keypad", classic);
    keypadToggle.setAttribute("aria-pressed", String(classic));
  };
  playerView.classList.toggle("touch-input", touchInput);
  showClassicKeypad(!touchInput && configStore.get("classicKeypad"));
  const showToolbar = () => {
    playerView.classList.remove("toolbar-hidden");
    clearTimeout(toolbarTimer);
    toolbarTimer = setTimeout(() => playerView.classList.add("toolbar-hidden"), TOOLBAR_HIDE_DELAY_MS);
  };
  const hideToolbar = () => {
    clearTimeout(toolbarTimer);
    playerView.classList.add("toolbar-hidden");
  };
  showToolbar();

  backToLibrary.addEventListener("click", () => { void finish(); }, { signal: abortController.signal });
  appSettings.addEventListener("click", () => {
    releaseKeys();
    settings.open();
  }, { signal: abortController.signal });
  // Tapping the game shows or hides the toolbar; touching the toolbar keeps it visible a while longer.
  playerOutput.addEventListener("pointerdown", () => {
    if (playerView.classList.contains("toolbar-hidden")) showToolbar();
    else hideToolbar();
  }, { signal: abortController.signal });
  playerToolbar.addEventListener("pointerdown", showToolbar, { signal: abortController.signal });
  keypadToggle.addEventListener("click", () => {
    releaseKeys();
    const classic = !playerView.classList.contains("classic-keypad");
    configStore.set("classicKeypad", classic);
    showClassicKeypad(classic);
  }, { signal: abortController.signal });
  window.addEventListener("blur", releaseKeys, { signal: abortController.signal });
  window.addEventListener("pagehide", () => { void finish(); }, { signal: abortController.signal });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) releaseKeys();
  }, { signal: abortController.signal });

  for (const button of buttons) {
    const key = button.dataset.key!;
    button.addEventListener(
      "pointerdown",
      (event) => {
        event.preventDefault();
        button.setPointerCapture(event.pointerId);
        setKey(`pointer:${event.pointerId}`, key);
      },
      { signal: abortController.signal },
    );
    const releaseKey = (event: PointerEvent) => {
      event.preventDefault();
      setKey(`pointer:${event.pointerId}`);
    };
    button.addEventListener("pointerup", releaseKey, { signal: abortController.signal });
    button.addEventListener("pointercancel", releaseKey, { signal: abortController.signal });
    button.addEventListener("lostpointercapture", releaseKey, { signal: abortController.signal });
  }

  // The pressed direction follows the finger, so it can slide between arms without lifting.
  const moveDpad = (event: PointerEvent) => {
    const rect = dpad.getBoundingClientRect();
    const radius = rect.width / 2;
    const dx = event.clientX - rect.left - radius;
    const dy = event.clientY - rect.top - radius;
    let key: string | undefined;
    if (Math.hypot(dx, dy) >= radius * DPAD_DEAD_ZONE) {
      if (Math.abs(dx) > Math.abs(dy)) key = dx > 0 ? "RIGHT" : "LEFT";
      else key = dy > 0 ? "DOWN" : "UP";
    }
    if (key) dpad.dataset.pressed = key;
    else delete dpad.dataset.pressed;
    setKey("dpad", key);
  };
  const releaseDpad = (event: PointerEvent) => {
    if (event.pointerId !== dpadPointer) return;
    event.preventDefault();
    setKey("dpad");
    resetDpad();
  };
  dpad.addEventListener(
    "pointerdown",
    (event) => {
      event.preventDefault();
      if (dpadPointer !== undefined) return;
      dpad.setPointerCapture(event.pointerId);
      dpadPointer = event.pointerId;
      moveDpad(event);
    },
    { signal: abortController.signal },
  );
  dpad.addEventListener(
    "pointermove",
    (event) => {
      if (event.pointerId === dpadPointer) moveDpad(event);
    },
    { signal: abortController.signal },
  );
  dpad.addEventListener("pointerup", releaseDpad, { signal: abortController.signal });
  dpad.addEventListener("pointercancel", releaseDpad, { signal: abortController.signal });
  dpad.addEventListener("lostpointercapture", releaseDpad, { signal: abortController.signal });

  // The stylesheet turns the touch-mode player sideways while the phone is upright.
  const portrait = window.matchMedia("(orientation: portrait)");
  // Maps a pointer to screen pixels of the centered, aspect-preserving canvas image, undoing the sideways layout.
  const canvasPoint = (event: PointerEvent): [number, number] | undefined => {
    const canvas = document.getElementById("canvas") as HTMLCanvasElement | null;
    if (!canvas || canvas.width === 0 || canvas.height === 0) return undefined;
    const rect = canvas.getBoundingClientRect();
    const [u, v, width, height] = portrait.matches
      ? [event.clientY - rect.top, rect.right - event.clientX, rect.height, rect.width]
      : [event.clientX - rect.left, event.clientY - rect.top, rect.width, rect.height];
    if (width === 0 || height === 0) return undefined;
    const scale = Math.min(width / canvas.width, height / canvas.height);
    const x = (u - (width - canvas.width * scale) / 2) / scale;
    const y = (v - (height - canvas.height * scale) / 2) / scale;
    return [Math.min(Math.max(Math.floor(x), -1), canvas.width), Math.min(Math.max(Math.floor(y), -1), canvas.height)];
  };
  const insideCanvas = ([x, y]: [number, number]) => {
    const canvas = document.getElementById("canvas") as HTMLCanvasElement;
    return x >= 0 && y >= 0 && x < canvas.width && y < canvas.height;
  };
  const clampToCanvas = ([x, y]: [number, number]): [number, number] => {
    const canvas = document.getElementById("canvas") as HTMLCanvasElement;
    return [Math.min(Math.max(x, 0), canvas.width - 1), Math.min(Math.max(y, 0), canvas.height - 1)];
  };
  const sendTouch = (phase: "down" | "move" | "up", point: [number, number]) => {
    if (!session?.touch || preparing || suspended || ending) return;
    void session.touch(phase, point[0], point[1]).catch(finish);
  };
  if (touchInput) {
    // Capture-phase listener so a touch on the game image does not also toggle the toolbar.
    playerOutput.addEventListener(
      "pointerdown",
      (event) => {
        const point = canvasPoint(event);
        if (touchPointer !== undefined || !point || !insideCanvas(point)) return;
        event.preventDefault();
        event.stopPropagation();
        playerOutput.setPointerCapture(event.pointerId);
        touchPointer = event.pointerId;
        touchPoint = point;
        sendTouch("down", point);
      },
      { capture: true, signal: abortController.signal },
    );
    playerOutput.addEventListener(
      "pointermove",
      (event) => {
        if (event.pointerId !== touchPointer) return;
        const point = canvasPoint(event);
        if (!point) return;
        const clamped = clampToCanvas(point);
        if (touchPoint && clamped[0] === touchPoint[0] && clamped[1] === touchPoint[1]) return;
        touchPoint = clamped;
        sendTouch("move", clamped);
      },
      { signal: abortController.signal },
    );
    const releaseTouch = (event: PointerEvent) => {
      if (event.pointerId !== touchPointer) return;
      touchPointer = undefined;
      const point = canvasPoint(event);
      sendTouch("up", point ? clampToCanvas(point) : touchPoint ?? [0, 0]);
      touchPoint = undefined;
    };
    playerOutput.addEventListener("pointerup", releaseTouch, { signal: abortController.signal });
    playerOutput.addEventListener("pointercancel", releaseTouch, { signal: abortController.signal });
    playerOutput.addEventListener("lostpointercapture", releaseTouch, { signal: abortController.signal });
  }

  document.addEventListener(
    "keydown",
    (event) => {
      if (event.target instanceof HTMLElement && event.target.closest("dialog")) {
        return;
      }

      const key = KEY_MAP[event.code];
      if (key) {
        event.preventDefault();
        if (!event.repeat) {
          setKey(`keyboard:${event.code}`, key);
        }
      }
    },
    { signal: abortController.signal },
  );
  document.addEventListener(
    "keyup",
    (event) => {
      const key = KEY_MAP[event.code];
      if (key) {
        event.preventDefault();
        setKey(`keyboard:${event.code}`);
      }
    },
    { signal: abortController.signal },
  );

  updateControls();
  start = backend.startApp(app.id, event => {
    if (ending) return;
    switch (event.type) {
      case "ready":
        if (preparing) recordPageView(app.title);
        preparing = false;
        updateControls();
        break;
      case "suspended":
        suspended = event.suspended;
        inputs.clear();
        updateControls();
        break;
      case "warning":
        clearTimeout(warningTimer);
        playerWarning.textContent = event.message;
        playerWarning.hidden = false;
        warningTimer = setTimeout(() => { playerWarning.hidden = true; }, 5000);
        break;
      case "stopped":
        void finish(undefined, event.failed);
        break;
      case "error":
        void finish(event.cause ?? new Error(event.message));
        break;
    }
  });
  void start.then(started => {
    session = started;
    updateControls();
  }).catch(finish);
});
