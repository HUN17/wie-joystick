import { extractAppMetadata, migrateSaves } from "@pkg";

import { AppLibraryStore } from "../app_library_store";
import type { LibraryApp, Wie } from "../backend";
import { configStore } from "../config_store";
import { t } from "../i18n";
import { setMasterVolume, setPcmVolume } from "../midi";
import { startWasmSession } from "./wasm";

const openDatabase = (name: string): Promise<IDBDatabase> =>
  new Promise((resolve, reject) => {
    const request = indexedDB.open(name);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });

// Every key and value of an object store, or nothing when the database has no such store.
const readStore = async (database: string, store: string): Promise<[IDBValidKey, unknown][]> => {
  const db = await openDatabase(database);
  try {
    if (!db.objectStoreNames.contains(store)) return [];
    return await new Promise((resolve, reject) => {
      const transaction = db.transaction(store, "readonly");
      const keys = transaction.objectStore(store).getAllKeys();
      const values = transaction.objectStore(store).getAll();
      transaction.oncomplete = () => resolve(keys.result.map((key, index) => [key, values.result[index]]));
      transaction.onerror = () => reject(transaction.error);
    });
  } finally {
    db.close();
  }
};

const toBase64 = (data: Uint8Array): string => {
  let binary = "";
  for (let offset = 0; offset < data.length; offset += 0x8000) {
    binary += String.fromCharCode(...data.subarray(offset, offset + 0x8000));
  }
  return btoa(binary);
};

export const initializeWie = async (): Promise<Wie> => {
  const store = await AppLibraryStore.open();

  const backend: Wie = {
    rendering: "canvas",
    listApps() {
      return store.list();
    },
    async importApp(file) {
      const original = new Uint8Array(await file.arrayBuffer());
      const extracted = extractAppMetadata(file.name, original);
      try {
        const archive = extracted.archive ?? original;
        const icon = extracted.icon;
        const app: LibraryApp = {
          id: extracted.id,
          title: extracted.title,
          filename: extracted.filename,
          addedAt: Date.now(),
          icon: icon.length ? new Blob([new Uint8Array(icon).buffer]) : undefined,
        };
        try {
          await store.add(app, archive);
        } catch (error) {
          if (error instanceof DOMException && error.name === "ConstraintError") {
            throw new Error(t("appAlreadyAdded", { title: app.title }));
          }
          throw error;
        }
      } finally {
        extracted.free();
      }
    },
    deleteApp(id) {
      return store.delete(id);
    },
    updateApp(app) {
      return store.update(app);
    },
    async originalAppMetadata(id) {
      const [app, archive] = await Promise.all([store.getApp(id), store.getArchive(id)]);
      if (!app || !archive) throw new Error(t("appFileMissing"));
      const extracted = extractAppMetadata(app.filename, archive);
      try {
        const icon = extracted.icon;
        return { title: extracted.title, icon: icon.length ? new Blob([new Uint8Array(icon).buffer]) : undefined };
      } finally {
        extracted.free();
      }
    },
    async exportSaves(id) {
      const app = await store.getApp(id);
      if (!app) throw new Error(t("appFileMissing"));
      const files = (await readStore("wie_filesystem", "files"))
        .filter(([key]) => Array.isArray(key) && key[0] === id)
        .map(([key, value]) => ({ path: String((key as IDBValidKey[])[1]), data: toBase64(new Uint8Array(value as Uint8Array)) }));
      const databases = (await indexedDB.databases()).some(db => db.name === `wie_${id}`)
        ? (await readStore(`wie_${id}`, `wie_${id}`)).map(([key, value]) => ({ key: String(key), data: toBase64(new Uint8Array(value as Uint8Array)) }))
        : [];
      if (!files.length && !databases.length) return undefined;
      const bundle = { app: id, title: app.title, exportedAt: new Date().toISOString(), files, records: databases };
      return new File([JSON.stringify(bundle, null, 1)], `${app.title}-saves.json`, { type: "application/json" });
    },
    async importSaves(id, file) {
      const bundle = JSON.parse(await file.text()) as { app?: unknown; files?: { path: string; data: string }[]; records?: unknown[] };
      if (bundle.app !== id || !Array.isArray(bundle.files)) throw new Error(t("savesMismatch"));
      if (bundle.records?.length) throw new Error(t("savesRecordsUnsupported"));
      const db = await openDatabase("wie_filesystem");
      try {
        await new Promise<void>((resolve, reject) => {
          const transaction = db.transaction("files", "readwrite");
          const files = transaction.objectStore("files");
          // Files missing from the bundle are removed, so the app sees exactly the exported saves.
          files.delete(IDBKeyRange.bound([id], [id, []]));
          for (const saved of bundle.files!) {
            files.put(Uint8Array.from(atob(saved.data), c => c.charCodeAt(0)), [id, saved.path]);
          }
          transaction.oncomplete = () => resolve();
          transaction.onerror = () => reject(transaction.error);
        });
      } finally {
        db.close();
      }
    },
    async readSettings() {
      return {
        midiVolume: configStore.get("midiVolume"),
        pcmVolume: configStore.get("pcmVolume"),
        helpDismissed: configStore.get("helpDismissed"),
        welcomeSeen: configStore.get("welcomeSeen"),
      };
    },
    async writeSettings(settings) {
      configStore.set("midiVolume", settings.midiVolume);
      configStore.set("pcmVolume", settings.pcmVolume);
      configStore.set("helpDismissed", settings.helpDismissed);
      configStore.set("welcomeSeen", settings.welcomeSeen);
      setMasterVolume(settings.midiVolume);
      setPcmVolume(settings.pcmVolume);
    },
    async startApp(id, onEvent) {
      const [app, archive, settings, fontResponse] = await Promise.all([
        store.getApp(id),
        store.getArchive(id),
        backend.readSettings(),
        fetch(new URL("../../../../assets/neodgm.ttf", import.meta.url)),
      ]);
      if (!app || !archive) throw new Error(t("appFileMissing"));
      if (!fontResponse.ok) throw new Error(`Failed to load font: ${fontResponse.status} ${fontResponse.statusText}`);
      const fontData = new Uint8Array(await fontResponse.arrayBuffer());
      await migrateSaves(app.filename, archive, app.id);
      setMasterVolume(settings.midiVolume);
      setPcmVolume(settings.pcmVolume);
      return startWasmSession(app.filename, archive, fontData, onEvent);
    },
  };
  return backend;
};
