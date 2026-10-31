import { WieWeb } from "@pkg";

import type { PlayerSession, SessionEvent, TouchPhase } from "../backend";
import { AudioPlayer } from "../midi";
import { rustPanic } from "../panic";
import type { EmulatorHost } from "./tauri-host";

export interface WasmSession extends PlayerSession {
  suspend(suspended: boolean): void;
}

export const startWasmSession = (
  filename: string,
  archive: Uint8Array,
  font: Uint8Array,
  onEvent: (event: SessionEvent) => void,
  host?: EmulatorHost,
  suspended = false,
): WasmSession => {
  if (rustPanic) throw rustPanic;
  const canvas = document.getElementById("canvas") as HTMLCanvasElement;
  canvas.width = 240;
  canvas.height = 320;
  const audio = host?.audio ?? new AudioPlayer();
  let emulator: WieWeb;
  try {
    emulator = new WieWeb(filename, archive, canvas, font, audio, host);
  } catch (error) {
    audio.dispose();
    throw rustPanic ?? error;
  }
  const keys = new Set<string>();
  const touchPhases: Record<TouchPhase, number> = { down: 0, move: 1, up: 2 };
  let touching = false;
  let touchX = 0;
  let touchY = 0;
  let running = true;
  let preparing = true;
  let frame: number;

  const session: WasmSession = {
    async key(key, pressed) {
      if (!running || preparing || suspended) return;
      if (pressed && !keys.has(key)) {
        emulator.key_down(key);
        keys.add(key);
      } else if (!pressed && keys.delete(key)) {
        emulator.key_up(key);
      }
    },
    async touch(phase, x, y) {
      if (!running || preparing || suspended) return;
      // Moves and releases only follow a press the emulator has seen.
      if (phase !== "down" && !touching) return;
      touching = phase !== "up";
      touchX = Math.round(x);
      touchY = Math.round(y);
      emulator.touch(touchPhases[phase], touchX, touchY);
    },
    async releaseKeys() {
      if (!running) return;
      for (const key of keys) emulator.key_up(key);
      keys.clear();
      if (touching) {
        touching = false;
        emulator.touch(touchPhases.up, touchX, touchY);
      }
    },
    suspend(paused) {
      if (!running) return;
      suspended = paused;
      for (const key of keys) emulator.key_up(key);
      keys.clear();
      if (touching) {
        touching = false;
        emulator.touch(touchPhases.up, touchX, touchY);
      }
      cancelAnimationFrame(frame);
      onEvent({ type: "suspended", suspended });
      if (!suspended) frame = requestAnimationFrame(update);
    },
    async stop() {
      if (!running) return;
      running = false;
      cancelAnimationFrame(frame);
      // Aborting panics do not unwind Rust borrows; only JavaScript resources can be released.
      if (rustPanic) audio.dispose();
      else emulator.free();
      onEvent({ type: "stopped", failed: false });
    },
  };

  const update = () => {
    if (!running || suspended) return;
    try {
      emulator.update();
      if (emulator.is_exited()) {
        void session.stop();
        return;
      }
      if (preparing && !emulator.is_preparing()) {
        preparing = false;
        onEvent({ type: "ready" });
      }
      frame = requestAnimationFrame(update);
    } catch (error) {
      const cause = rustPanic ?? error;
      onEvent({ type: "error", message: String(cause), cause });
    }
  };
  onEvent({ type: "suspended", suspended });
  if (!suspended) frame = requestAnimationFrame(() => { frame = requestAnimationFrame(update); });
  return session;
};
