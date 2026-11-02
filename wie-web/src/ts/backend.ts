import { isTauri } from "@tauri-apps/api/core";

export interface LibraryApp {
  id: string;
  title: string;
  filename: string;
  icon?: Blob;
  addedAt: number;
}

export interface Settings {
  midiVolume: number;
  pcmVolume: number;
  helpDismissed: boolean;
  welcomeSeen: boolean;
}

export type SessionEvent =
  | { type: "ready" }
  | { type: "suspended"; suspended: boolean }
  | { type: "warning"; message: string }
  | { type: "stopped"; failed: boolean }
  | { type: "error"; message: string; cause?: unknown };

export type TouchPhase = "down" | "move" | "up";

export interface PlayerSession {
  key(key: string, pressed: boolean): Promise<void>;
  /** Single-pointer touch in screen pixels; absent when the runtime cannot route touches. */
  touch?(phase: TouchPhase, x: number, y: number): Promise<void>;
  releaseKeys(): Promise<void>;
  stop(): Promise<void>;
}

export interface Wie {
  readonly rendering: "canvas" | "native";
  listApps(): Promise<LibraryApp[]>;
  importApp(file: File): Promise<void>;
  deleteApp(id: string): Promise<void>;
  /** Saves an edited title or icon; absent when the backend cannot edit apps. */
  updateApp?(app: LibraryApp): Promise<void>;
  /** The title and icon read from the app file itself. */
  originalAppMetadata?(id: string): Promise<{ title: string; icon?: Blob }>;
  /** The app's saved files and records bundled into one JSON file; absent when saves live elsewhere. */
  exportSaves?(id: string): Promise<File | undefined>;
  /** Replaces the app's saved files with those in a bundle made by exportSaves. */
  importSaves?(id: string, file: File): Promise<void>;
  readSettings(): Promise<Settings>;
  writeSettings(settings: Settings): Promise<void>;
  startApp(id: string, onEvent: (event: SessionEvent) => void): Promise<PlayerSession | undefined>;
}

export const initializeWie = async (): Promise<Wie> => {
  const backend = isTauri()
    ? await import("./backend/tauri")
    : await import("./backend/browser");

  return backend.initializeWie();
};
