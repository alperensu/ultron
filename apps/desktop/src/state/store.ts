import { createContext, useContext, useSyncExternalStore } from "react";
import {
  describeBridgeError,
  type Bridge,
  type ChatInfo,
  type EngineId,
  type ProjectInfo,
  type QuotaReport,
  type StoredEvent,
  type UpdateChatPatch,
} from "../lib/bridge";

const DAY_MS = 24 * 60 * 60 * 1000;
/** Sen yokken taraması: en fazla bu kadar sohbet ve sohbet başına bu kadar sayfa okunur. */
const SCAN_CHATS = 12;
const SCAN_PAGES = 10;
const SCAN_PAGE_SIZE = 500;

/** Son 24 saatte biten bir tur (`turn_completed` olayından). */
export interface CompletedTurn {
  eventId: string;
  chatId: string;
  chatTitle: string;
  engine: EngineId | null;
  finishedAt: string;
  /** Turu başlatan kullanıcı mesajından bitişe kadar; mesaj bulunamadıysa null. */
  durationMs: number | null;
}

export type DaemonStatus =
  | { state: "connecting" }
  | { state: "ready"; version: string; runningTurns: number }
  | { state: "unavailable"; reason: string };

export interface AppState {
  loaded: boolean;
  daemon: DaemonStatus;
  projects: ProjectInfo[];
  chats: ChatInfo[];
  quota: QuotaReport | null;
  /** Kota okunamadıysa kullanıcıya gösterilecek neden. */
  quotaError: string | null;
  turns: CompletedTurn[];
  /** Kenar çubuğunda açık olan proje kimlikleri. */
  expandedProjects: string[];
  /** En son tek kutudan gönderilen mesaj; sohbet ekranına paylaşılan öğe animasyonu için. */
  lastSent: { chatId: string; text: string } | null;
}

export interface AppStore {
  readonly bridge: Bridge;
  getState(): AppState;
  subscribe(listener: () => void): () => void;
  /** İlk yükleme ve canlı olay aboneliği; dönen işlev aboneliği bırakır. */
  start(): () => void;
  updateChat(chatId: string, patch: UpdateChatPatch): Promise<void>;
  /** Yeni sohbet açıp ilk mesajı gönderir; sohbet kimliğini döndürür. */
  createChat(text: string, projectId: string | null): Promise<string>;
  toggleProject(projectId: string): void;
  expandProject(projectId: string): void;
}

export function createAppStore(bridge: Bridge, now: () => number = Date.now): AppStore {
  let state: AppState = {
    loaded: false,
    daemon: { state: "connecting" },
    projects: [],
    chats: [],
    quota: null,
    quotaError: null,
    turns: [],
    expandedProjects: [],
    lastSent: null,
  };
  const listeners = new Set<() => void>();
  /** Canlı olaylardan süre hesabı için sohbet başına son kullanıcı mesajı zamanı. */
  const lastUserTs = new Map<string, string>();

  const set = (patch: Partial<AppState>) => {
    state = { ...state, ...patch };
    listeners.forEach((listener) => listener());
  };

  const sortTurns = (turns: CompletedTurn[]) =>
    turns
      .filter((t) => now() - Date.parse(t.finishedAt) <= DAY_MS)
      .sort((a, b) => Date.parse(b.finishedAt) - Date.parse(a.finishedAt));

  async function refreshChats(): Promise<void> {
    const { chats } = await bridge.listChats();
    set({ chats: chats ?? [] });
  }

  async function scanTurns(chats: ChatInfo[]): Promise<CompletedTurn[]> {
    const recent = chats
      .filter((c) => now() - Date.parse(c.updated_at) <= DAY_MS)
      .sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at))
      .slice(0, SCAN_CHATS);
    const found: CompletedTurn[] = [];
    for (const chat of recent) {
      let after: number | null = null;
      let userTs: string | null = null;
      for (let page = 0; page < SCAN_PAGES; page++) {
        const res = await bridge.eventsPage(chat.id, after, SCAN_PAGE_SIZE);
        for (const ev of res.events ?? []) {
          if (ev.kind === "user_text") userTs = ev.ts;
          if (ev.kind === "turn_completed") found.push(toTurn(chat, ev, userTs));
        }
        if (!res.has_more || res.next_after_seq === null) break;
        after = res.next_after_seq;
      }
    }
    return found;
  }

  function toTurn(chat: ChatInfo, ev: StoredEvent, userTs: string | null): CompletedTurn {
    return {
      eventId: ev.id,
      chatId: chat.id,
      chatTitle: chat.title,
      engine: chat.engine,
      finishedAt: ev.ts,
      durationMs: userTs ? Date.parse(ev.ts) - Date.parse(userTs) : null,
    };
  }

  function onLiveEvent(ev: StoredEvent): void {
    if (ev.kind === "user_text") {
      lastUserTs.set(ev.chat_id, ev.ts);
      return;
    }
    if (ev.kind !== "turn_completed") return;
    const chat = state.chats.find((c) => c.id === ev.chat_id);
    if (!chat) {
      void refreshChats().catch(() => undefined);
      return;
    }
    if (state.turns.some((t) => t.eventId === ev.id)) return;
    const turn = toTurn(chat, ev, lastUserTs.get(ev.chat_id) ?? null);
    set({ turns: sortTurns([turn, ...state.turns]) });
  }

  async function load(): Promise<void> {
    try {
      const [status, projects, chats] = await Promise.all([
        bridge.status(),
        bridge.listProjects(),
        bridge.listChats(),
      ]);
      set({
        loaded: true,
        daemon: { state: "ready", version: status.version, runningTurns: status.running_turns },
        projects: projects.projects ?? [],
        chats: chats.chats ?? [],
      });
    } catch (err) {
      set({ loaded: true, daemon: { state: "unavailable", reason: describeBridgeError(err) } });
      return;
    }
    // Kota ve tur taraması başarısız olursa arayüz çökmez; dürüst boş durum gösterilir.
    await Promise.all([
      bridge.getQuota().then(
        (quota) => set({ quota, quotaError: null }),
        (err: unknown) => set({ quota: null, quotaError: describeBridgeError(err) }),
      ),
      scanTurns(state.chats).then(
        (turns) => set({ turns: sortTurns(turns) }),
        () => undefined,
      ),
    ]);
  }

  return {
    bridge,
    getState: () => state,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    start() {
      let stopped = false;
      const unsubscribers: Array<() => void> = [];
      const track = (pending: Promise<() => void>) =>
        pending.then((off) => (stopped ? off() : unsubscribers.push(off)));
      void track(bridge.onEvent(onLiveEvent));
      void track(
        bridge.onDaemonState((s) => {
          if (s.state === "ready") void load();
          else set({ daemon: { state: "unavailable", reason: describeBridgeError(s.error) } });
        }),
      );
      void load();
      return () => {
        stopped = true;
        unsubscribers.forEach((off) => off());
      };
    },
    async updateChat(chatId, patch) {
      const { chat } = await bridge.updateChat(chatId, patch);
      set({
        chats: chat.archived
          ? state.chats.filter((c) => c.id !== chat.id)
          : state.chats.map((c) => (c.id === chat.id ? chat : c)),
      });
    },
    async createChat(text, projectId) {
      const { chat_id } = await bridge.sendMessage({
        text,
        ...(projectId ? { project_id: projectId } : {}),
      });
      set({ lastSent: { chatId: chat_id, text } });
      await refreshChats();
      return chat_id;
    },
    toggleProject(projectId) {
      const open = state.expandedProjects.includes(projectId);
      set({
        expandedProjects: open
          ? state.expandedProjects.filter((id) => id !== projectId)
          : [...state.expandedProjects, projectId],
      });
    },
    expandProject(projectId) {
      if (!state.expandedProjects.includes(projectId)) {
        set({ expandedProjects: [...state.expandedProjects, projectId] });
      }
    },
  };
}

// --- React bağlantısı ---

const StoreContext = createContext<AppStore | null>(null);
export const StoreProvider = StoreContext.Provider;

export function useStore(): AppStore {
  const store = useContext(StoreContext);
  if (!store) throw new Error("StoreProvider eksik");
  return store;
}

export function useAppState<T>(selector: (state: AppState) => T): T {
  const store = useStore();
  return useSyncExternalStore(store.subscribe, () => selector(store.getState()));
}
