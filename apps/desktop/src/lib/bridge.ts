// Tauri köprüsü: Rust tiplerinin aynası + sahte köprü (UI testleri için).
// Webview yalnızca Tauri komutları ve `ultron://event` ile konuşur; token burada yoktur.

export type EngineId = "claude" | "codex" | "agy";

export interface ProjectInfo {
  id: string;
  name: string;
  path: string;
  aliases: string[];
  description: string | null;
  last_active_at: string | null;
}

export interface ChatInfo {
  id: string;
  title: string;
  project_id: string | null;
  pinned: boolean;
  archived: boolean;
  updated_at: string;
  engine: EngineId | null;
}

export interface ChatDetail {
  chat: ChatInfo;
  work_state: unknown;
  last_engine: EngineId | null;
}

export interface StoredEvent {
  id: string;
  chat_id: string;
  session_id: string | null;
  seq: number;
  ts: string;
  kind: string;
  payload: unknown;
}

export interface EventsPage {
  events: StoredEvent[];
  next_after_seq: number | null;
  has_more: boolean;
}

/** Tam metin arama sonucu (Rust `SearchHit`). */
export interface SearchHit {
  chat_id: string;
  event_id: string;
  text: string;
}

/** Yerel proje yönlendirmesi sonucu (Rust `RouteResult`); alternatifler proje kimlikleridir. */
export interface RouteResult {
  project_id: string | null;
  confidence: number;
  reason: string;
  alternatives: string[];
}

export type QuotaSource = "Live" | "Estimated" | "Unknown";

export interface QuotaWindow {
  label: string;
  used_pct: number | null;
  resets_at: string | null;
  window_secs: number;
  source: QuotaSource;
}

export interface PoolReport {
  pool: "claude" | "codex" | "agy";
  windows: QuotaWindow[];
  reset_rights: number | null;
  mode: string;
  note?: string;
}

export interface QuotaReport {
  pools: PoolReport[];
  fetched_at: string;
}

export interface ImportReport {
  files: number;
  chats: number;
  messages: number;
  skipped: number;
  masked: number;
}

/** `importCatalog` sonucu (Rust `CatalogImportReport`). */
export interface CatalogImportReport {
  imported: number;
  updated: number;
}

export interface SendResult {
  chat_id: string;
}

/** Köprü hataları (Rust `BridgeError`). Servis hatası mesajı aynen gelir. */
export type BridgeError =
  | { kind: "daemon_unavailable"; reason: string }
  | { kind: "daemon"; message: string }
  | { kind: "protocol"; message: string };

/** Servis durumu olayı (`ultron://daemon`). */
export type DaemonState =
  | { state: "ready" }
  | { state: "unavailable"; error: BridgeError };

export const EVENT_NAME = "ultron://event";
export const DAEMON_EVENT = "ultron://daemon";

export interface UpdateChatPatch {
  title?: string;
  pinned?: boolean;
  archived?: boolean;
  /** `null`: projeden ayır; metin: projeye bağla; alan yok: dokunma. */
  project_id?: string | null;
}

export interface SendRequest {
  chat_id?: string;
  text: string;
  engine?: EngineId;
  project_id?: string;
}

export interface Bridge {
  listProjects(): Promise<{ projects: ProjectInfo[] }>;
  listChats(args?: { project_id?: string; include_archived?: boolean }): Promise<{ chats: ChatInfo[] }>;
  getChat(chatId: string): Promise<ChatDetail>;
  updateChat(chatId: string, patch: UpdateChatPatch): Promise<{ chat: ChatInfo }>;
  sendMessage(req: SendRequest): Promise<SendResult>;
  switchEngine(chatId: string, to: EngineId): Promise<{ chat_id: string }>;
  cancelTurn(chatId: string): Promise<{ chat_id: string }>;
  eventsPage(chatId: string, afterSeq: number | null, limit: number): Promise<EventsPage>;
  historySearch(query: string, chatId?: string, limit?: number): Promise<{ hits: SearchHit[] }>;
  getQuota(): Promise<QuotaReport>;
  setResetRights(pool: string, count: number): Promise<unknown>;
  routeMessage(text: string): Promise<RouteResult>;
  importCatalog(path: string): Promise<CatalogImportReport>;
  importClaudeHistory(root?: string): Promise<ImportReport>;
  status(): Promise<{ version: string; pid: number; running_turns: number; default_engine: EngineId }>;
  /** `ultron://event` aboneliği; dönen işlev aboneliği bırakır. */
  onEvent(handler: (ev: StoredEvent) => void): Promise<() => void>;
  /** `ultron://daemon` aboneliği. */
  onDaemonState(handler: (s: DaemonState) => void): Promise<() => void>;
}

// --- Tip yardımcıları ---

export function isBridgeError(value: unknown): value is BridgeError {
  if (typeof value !== "object" || value === null) return false;
  const kind = (value as { kind?: unknown }).kind;
  return kind === "daemon_unavailable" || kind === "daemon" || kind === "protocol";
}

/** Bilinmeyen bir hatayı (Tauri reddi, Error, metin) `BridgeError`'a indirger. */
export function toBridgeError(err: unknown): BridgeError {
  if (isBridgeError(err)) return err;
  const message = err instanceof Error ? err.message : typeof err === "string" ? err : "bilinmeyen hata";
  return { kind: "protocol", message };
}

export function isDaemonUnavailable(err: unknown): err is Extract<BridgeError, { kind: "daemon_unavailable" }> {
  return isBridgeError(err) && err.kind === "daemon_unavailable";
}

/** Kullanıcıya gösterilecek kısa hata metni; servis hatası mesajı aynen geçer. */
export function describeBridgeError(err: unknown): string {
  const e = toBridgeError(err);
  switch (e.kind) {
    case "daemon_unavailable":
      return `Servis başlatılamadı: ${e.reason}`;
    case "daemon":
    case "protocol":
      return e.message;
  }
}

/** Olay yükü `{ type: <kind>, ... }` biçimindeyse ilgili tip için daraltır. */
export function payloadOf<T extends Record<string, unknown>>(ev: StoredEvent, kind: string): T | null {
  if (ev.kind !== kind) return null;
  return typeof ev.payload === "object" && ev.payload !== null ? (ev.payload as T) : null;
}

/** `after_seq`'ten sonraki yeni olayları ekler; `seq` tekrarlarını eler, sıralı tutar. */
export function mergeEvents(existing: StoredEvent[], incoming: StoredEvent[]): StoredEvent[] {
  const last = existing.length ? existing[existing.length - 1]!.seq : 0;
  const fresh = incoming.filter((e) => e.seq > last).sort((a, b) => a.seq - b.seq);
  const out = existing.slice();
  for (const ev of fresh) {
    if (out.length === 0 || ev.seq > out[out.length - 1]!.seq) out.push(ev);
  }
  return out;
}

// --- Gerçek köprü (Tauri) ---

export function createTauriBridge(): Bridge {
  // Tauri API yalnız ihtiyaç anında yüklenir (tarayıcı/test ortamında import hatası olmasın).
  const call = async <T>(cmd: string, args?: Record<string, unknown>): Promise<T> => {
    const { invoke } = await import("@tauri-apps/api/core");
    try {
      return await invoke<T>(cmd, args === undefined ? undefined : { args });
    } catch (err) {
      throw toBridgeError(err);
    }
  };
  const listenTo = async <T>(name: string, handler: (p: T) => void): Promise<() => void> => {
    const { listen } = await import("@tauri-apps/api/event");
    return listen<T>(name, (e) => handler(e.payload));
  };
  return {
    listProjects: () => call("list_projects"),
    listChats: (args = {}) => call("list_chats", { include_archived: false, ...args }),
    getChat: (chat_id) => call("get_chat", { chat_id }),
    updateChat: (chat_id, patch) => call("update_chat", { chat_id, ...patch }),
    sendMessage: (req) => call("send_message", { ...req }),
    switchEngine: (chat_id, to) => call("switch_engine", { chat_id, to }),
    cancelTurn: (chat_id) => call("cancel_turn", { chat_id }),
    eventsPage: (chat_id, after_seq, limit) => call("events_page", { chat_id, after_seq, limit }),
    historySearch: (query, chat_id, limit = 20) => call("history_search", { query, chat_id, limit }),
    getQuota: () => call("get_quota"),
    setResetRights: (pool, count) => call("set_reset_rights", { pool, count }),
    routeMessage: (text) => call("route_message", { text }),
    importCatalog: (path) => call("import_catalog", { path }),
    importClaudeHistory: (root) => call("import_claude_history", { root }),
    status: () => call("status"),
    onEvent: (handler) => listenTo<StoredEvent>(EVENT_NAME, handler),
    onDaemonState: (handler) => listenTo<DaemonState>(DAEMON_EVENT, handler),
  };
}

// --- Sahte köprü (yalnız testler ve /__ui) ---

export interface FakeBridgeOptions {
  projects?: ProjectInfo[];
  chats?: ChatInfo[];
  events?: StoredEvent[];
  /** Verilirse her komut bu hatayla reddedilir (ör. DaemonUnavailable ekranı). */
  failWith?: BridgeError;
  /** Yönlendirme yanıtı; verilmezse `routeMessage` "uygulanmadı" hatası verir. */
  route?: (text: string) => RouteResult;
  /** Kota raporu; verilmezse `getQuota` "uygulanmadı" hatası verir. */
  quota?: QuotaReport;
  /** `historySearch` sonuçları (sorguyu içeren metinler süzülür). */
  searchHits?: SearchHit[];
  /** Yalnız testler: ilk abonelikle zamanlayıcıdan yayınlanan olaylar. */
  stream?: { events: StoredEvent[]; eventsPerSecond?: number; delayMs?: number };
}

export interface FakeBridge extends Bridge {
  /** Canlı olay yayar (test sürücüsü). */
  emit(ev: StoredEvent): void;
  emitDaemonState(state: DaemonState): void;
  /** Zamanlayıcıyı ve test aboneliklerini temizler. */
  dispose(): void;
  readonly calls: Array<{ cmd: string; args: unknown[] }>;
}

export function createFakeBridge(opts: FakeBridgeOptions = {}): FakeBridge {
  const chats = new Map((opts.chats ?? []).map((c) => [c.id, { ...c }]));
  const events = [...(opts.events ?? [])];
  const activeChats = new Set<string>();
  const trackTurn = (event: StoredEvent) => {
    if (["user_text", "session_started", "assistant_text"].includes(event.kind)) activeChats.add(event.chat_id);
    if (["turn_completed", "error", "quota_exhausted"].includes(event.kind)) activeChats.delete(event.chat_id);
  };
  events.forEach(trackTurn);
  const eventHandlers = new Set<(ev: StoredEvent) => void>();
  const daemonHandlers = new Set<(s: DaemonState) => void>();
  const calls: FakeBridge["calls"] = [];
  let streamIndex = 0;
  let streamTimer: ReturnType<typeof setTimeout> | undefined;
  let streamStarted = false;
  const emit = (event: StoredEvent) => {
    events.push(event);
    trackTurn(event);
    eventHandlers.forEach((handler) => handler(event));
  };
  const append = (chatId: string, kind: string, payload: Record<string, unknown>) => {
    const seq = events.reduce((last, event) => Math.max(last, event.seq), 0) + 1;
    emit({
      id: `fake-${chatId}-${seq}`, chat_id: chatId, session_id: null,
      seq, ts: new Date().toISOString(), kind, payload: { type: kind, ...payload },
    });
  };
  const scheduleStream = () => {
    const stream = opts.stream;
    if (!stream || streamTimer !== undefined || streamIndex >= stream.events.length || eventHandlers.size === 0) return;
    const rate = Math.max(1, Math.min(1000, stream.eventsPerSecond ?? 50));
    const delay = streamStarted ? 1000 / rate : Math.max(0, stream.delayMs ?? 1000 / rate);
    streamStarted = true;
    streamTimer = setTimeout(() => {
      streamTimer = undefined;
      const event = stream.events[streamIndex++];
      if (event) emit(event);
      scheduleStream();
    }, delay);
  };
  const notImplemented = (what: string): BridgeError => ({
    kind: "daemon",
    message: `henüz uygulanmadı (${what})`,
  });
  const reject = (what: string): never => {
    throw notImplemented(what);
  };
  const run = async <T>(cmd: string, args: unknown[], fn: () => T): Promise<T> => {
    calls.push({ cmd, args });
    if (opts.failWith) throw opts.failWith;
    return fn();
  };
  const getChat = (id: string): ChatInfo => {
    const c = chats.get(id);
    if (!c) throw { kind: "daemon", message: `sohbet bulunamadı: ${id}` } satisfies BridgeError;
    return c;
  };
  return {
    calls,
    emit,
    dispose: () => {
      clearTimeout(streamTimer);
      streamTimer = undefined;
      eventHandlers.clear();
      daemonHandlers.clear();
    },
    emitDaemonState: (s) => daemonHandlers.forEach((h) => h(s)),
    listProjects: () => run("list_projects", [], () => ({ projects: opts.projects ?? [] })),
    listChats: (args) =>
      run("list_chats", [args], () => ({
        chats: [...chats.values()].filter(
          (c) =>
            (args?.include_archived || !c.archived) &&
            (args?.project_id === undefined || c.project_id === args.project_id),
        ),
      })),
    getChat: (id) => run("get_chat", [id], () => ({ chat: { ...getChat(id) }, work_state: null, last_engine: getChat(id).engine })),
    updateChat: (id, patch) =>
      run("update_chat", [id, patch], () => {
        const c = getChat(id);
        if (patch.title !== undefined) c.title = patch.title;
        if (patch.pinned !== undefined) c.pinned = patch.pinned;
        if (patch.archived !== undefined) c.archived = patch.archived;
        if (patch.project_id !== undefined) c.project_id = patch.project_id;
        return { chat: { ...c } };
      }),
    sendMessage: (req) =>
      run("send_message", [req], () => {
        const id = req.chat_id ?? `sohbet-${chats.size + 1}`;
        if (!chats.has(id)) {
          chats.set(id, {
            id,
            title: req.text.slice(0, 60),
            project_id: req.project_id ?? null,
            pinned: false,
            archived: false,
            updated_at: new Date().toISOString(),
            engine: req.engine ?? "claude",
          });
        }
        append(id, "user_text", { text: req.text });
        return { chat_id: id };
      }),
    switchEngine: (id, to) =>
      run("switch_engine", [id, to], () => {
        const chat = getChat(id);
        if (activeChats.has(id)) throw { kind: "daemon", message: "sohbette çalışan bir tur var" } satisfies BridgeError;
        const from = chat.engine;
        chat.engine = to;
        append(id, "engine_switched", { from, to, reason: "user" });
        return { chat_id: id };
      }),
    cancelTurn: (id) => run("cancel_turn", [id], () => {
      getChat(id);
      // Gerçek servis iptal için turn_completed üretmez; durum sayacı sıfıra iner.
      activeChats.delete(id);
      return { chat_id: id };
    }),
    eventsPage: (id, after, limit) =>
      run("events_page", [id, after, limit], () => {
        const all = events.filter((e) => e.chat_id === id && e.seq > (after ?? 0)).sort((a, b) => a.seq - b.seq);
        const page = all.slice(0, limit);
        return {
          events: page,
          next_after_seq: page.length ? page[page.length - 1]!.seq : after,
          has_more: all.length > page.length,
        };
      }),
    historySearch: (query, id, limit) =>
      run("history_search", [query, id, limit], () => ({
        hits: (opts.searchHits ?? []).filter((h) => h.text.toLowerCase().includes(query.toLowerCase())),
      })),
    getQuota: () => run("get_quota", [], () => opts.quota ?? reject("kota")),
    setResetRights: (pool, count) =>
      run("set_reset_rights", [pool, count], () => reject("kota")),
    routeMessage: (text) =>
      run("route_message", [text], () => (opts.route ? opts.route(text) : reject("yönlendirme"))),
    // Sahte köprü: gerçek dosya okumaz; boş rapor, gerçek servisle aynı biçimde.
    importCatalog: (path) => run("import_catalog", [path], () => ({ imported: 0, updated: 0 })),
    importClaudeHistory: (root) =>
      run("import_claude_history", [root], () => ({ files: 0, chats: 0, messages: 0, skipped: 0, masked: 0 })),
    status: () =>
      run("status", [], () => ({ version: "test", pid: 0, running_turns: activeChats.size, default_engine: "claude" as const })),
    onEvent: async (h) => {
      eventHandlers.add(h);
      scheduleStream();
      return () => {
        eventHandlers.delete(h);
        if (eventHandlers.size === 0) {
          clearTimeout(streamTimer);
          streamTimer = undefined;
        }
      };
    },
    onDaemonState: async (h) => {
      daemonHandlers.add(h);
      return () => daemonHandlers.delete(h);
    },
  };
}
