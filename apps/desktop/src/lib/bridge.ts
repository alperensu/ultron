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

/** `importProjects` sonucu (Rust `ProjectImportReport`). */
export interface ProjectImportRow {
  path: string;
  name: string;
  claude_sessions: number;
  codex_sessions: number;
  worker_sessions: number;
  project_created: boolean;
  note_updated: boolean;
  note_status: string;
}
export interface ProjectImportReport {
  dry_run: boolean;
  rows: ProjectImportRow[];
  created: number;
  notes_updated: number;
  summarizer: string;
  unmatched_sessions: number;
}

/** `importCatalog` sonucu (Rust `CatalogImportReport`). */
export interface CatalogImportReport {
  imported: number;
  updated: number;
}

export interface SendResult {
  chat_id: string;
}

export interface ResetRightsResult {
  pool: string;
  count: number;
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
  elevated?: boolean;
}

export type ApprovalStatus = "pending" | "approved" | "denied" | "expired";
export type RiskCategory = "Delete" | "GitHistory" | "Deploy" | "Finance" | "Secrets" | "Install" | "SystemConfig" | "Publish";
/** Servisin maskelediği kalıcı kayıt; onaylayıcı sırrı bu sözleşmenin parçası değildir. */
export interface Approval {
  id: string;
  chat_id: string | null;
  engine: EngineId;
  category: RiskCategory;
  reason: string;
  action_json: string;
  status: ApprovalStatus;
  created_at: string;
  resolved_at: string | null;
  resolved_by: "desktop" | "telegram" | "approver" | "timeout" | "restart" | null;
}
export interface ApprovalPage { approvals: Approval[]; next_cursor: string | null }
export interface PolicyConfig {
  finance_paths: string[];
  protected_paths: string[];
  deploy_patterns: string[];
  allow_list: string[];
}
export interface PolicySettings { config: PolicyConfig; trash_retention_days: number }
export interface TelegramSettings {
  notify: "always" | "away";
  auto_continue_enabled: boolean;
  auto_continue_wait_minutes: number;
  fallback_order: EngineId[];
}
export type TelegramErrorCode =
  | "not_verified" | "invalid_token" | "credential_unavailable" | "network" | "timeout"
  | "unauthorized" | "poll_conflict" | "rate_limited" | "provider_unavailable"
  | "invalid_response" | "delivery_uncertain";
export interface TelegramStatusSnapshot {
  state: "connected" | "missing_token" | "missing_allowed_users" | "error";
  allowed_user_count: number;
  settings: TelegramSettings;
  error: TelegramErrorCode | null;
}
export interface TelegramTestReport {
  attempted: number;
  delivered: number;
  failed: number;
  uncertain: number;
}

export interface TrashEntry {
  id: string;
  original_path: string;
  deleted_at: string;
  chat_id: string | null;
  size: number;
  is_dir: boolean;
}

export interface Bridge {
  listApprovals(args?: { status?: ApprovalStatus; cursor?: string; limit?: number }): Promise<ApprovalPage>;
  resolveApproval(id: string, approve: boolean): Promise<{ approval: Approval }>;
  getPolicyConfig(): Promise<PolicySettings>;
  setPolicyConfig(settings: PolicySettings): Promise<PolicySettings>;
  telegramStatus(): Promise<TelegramStatusSnapshot>;
  getTelegramSettings(): Promise<TelegramSettings>;
  setTelegramSettings(settings: TelegramSettings): Promise<TelegramSettings>;
  telegramTest(): Promise<TelegramTestReport>;
  listTrash(): Promise<{ entries: TrashEntry[] }>;
  restoreTrash(id: string): Promise<{ path: string }>;
  purgeTrash(id: string): Promise<{ id: string }>;
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
  setResetRights(pool: string, count: number): Promise<ResetRightsResult>;
  routeMessage(text: string): Promise<RouteResult>;
  importCatalog(path: string): Promise<CatalogImportReport>;
  importProjects(): Promise<ProjectImportReport>;
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
    listApprovals: (args = {}) => call("list_approvals", args),
    resolveApproval: (id, approve) => call("resolve_approval", { id, approve }),
    getPolicyConfig: () => call("get_policy_config"),
    setPolicyConfig: (settings) => call("set_policy_config", { settings }),
    telegramStatus: () => call("telegram_status"),
    getTelegramSettings: () => call("get_telegram_settings"),
    setTelegramSettings: ({ notify, auto_continue_enabled, auto_continue_wait_minutes, fallback_order }) =>
      call("set_telegram_settings", { notify, auto_continue_enabled, auto_continue_wait_minutes, fallback_order }),
    telegramTest: () => call("telegram_test"),
    listTrash: () => call("list_trash"),
    restoreTrash: (id) => call("restore_trash", { id }),
    purgeTrash: (id) => call("purge_trash", { id }),
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
    importProjects: () => call("import_projects", {}),
    status: () => call("status"),
    onEvent: (handler) => listenTo<StoredEvent>(EVENT_NAME, handler),
    onDaemonState: (handler) => listenTo<DaemonState>(DAEMON_EVENT, handler),
  };
}

// --- Sahte köprü (yalnız testler ve /__ui) ---

export interface FakeBridgeOptions {
  approvals?: Approval[];
  policySettings?: PolicySettings;
  telegramStatus?: TelegramStatusSnapshot;
  telegramTestReport?: TelegramTestReport;
  trash?: TrashEntry[];
  projects?: ProjectInfo[];
  chats?: ChatInfo[];
  events?: StoredEvent[];
  /** Verilirse her komut bu hatayla reddedilir (ör. DaemonUnavailable ekranı). */
  failWith?: BridgeError;
  /** Yönlendirme yanıtı; verilmezse `routeMessage` "uygulanmadı" hatası verir. */
  route?: (text: string) => RouteResult;
  /** Kota raporu; verilmezse `getQuota` "uygulanmadı" hatası verir. */
  quota?: QuotaReport;
  /** Sentetik içe aktarma sonuçları; gerçek dosya okunmaz. */
  catalogImportReport?: CatalogImportReport;
  projectImportReport?: ProjectImportReport;
  /** Testlerde servis ayarlarının salt okunur görünümü. */
  defaultEngine?: EngineId;
  /** `historySearch` sonuçları (sorguyu içeren metinler süzülür). */
  searchHits?: SearchHit[];
  /** Yalnız testler: ilk abonelikle zamanlayıcıdan yayınlanan olaylar. */
  stream?: { events: StoredEvent[]; eventsPerSecond?: number; delayMs?: number };
  /** Yalnız sentetik test verisi: sohbetleri ve olayları bu localStorage anahtarında saklar. */
  persistenceKey?: string;
}

export interface FakeBridge extends Bridge {
  /** Sentetik kuyruğa kayıt ekleyip gerçek akışla aynı uyandırma olayını yayınlar. */
  addApproval(approval: Approval): void;
  /** Canlı olay yayar (test sürücüsü). */
  emit(ev: StoredEvent): void;
  emitDaemonState(state: DaemonState): void;
  /** Zamanlayıcıyı ve test aboneliklerini temizler. */
  dispose(): void;
  readonly calls: Array<{ cmd: string; args: unknown[] }>;
}

export function createFakeBridge(opts: FakeBridgeOptions = {}): FakeBridge {
  const approvals = new Map((opts.approvals ?? []).map((item) => [item.id, structuredClone(item)]));
  let policySettings = structuredClone(opts.policySettings ?? {
    config: { finance_paths: [], protected_paths: [], deploy_patterns: [], allow_list: [] }, trash_retention_days: 30,
  });
  const telegramStatus = opts.telegramStatus ? structuredClone(opts.telegramStatus) : undefined;
  const trash = new Map((opts.trash ?? []).map((item) => [item.id, structuredClone(item)]));
  const quota = opts.quota ? structuredClone(opts.quota) : undefined;
  let saved: { version: 1; chats: ChatInfo[]; events: StoredEvent[] } | undefined;
  if (opts.persistenceKey) {
    const raw = localStorage.getItem(opts.persistenceKey);
    if (raw) {
      // Eski ya da bozuk bir test kaydı başlangıç fikstürünün yüklenmesini engellemez.
      try {
        const parsed = JSON.parse(raw) as typeof saved;
        if (parsed?.version === 1 && Array.isArray(parsed.chats) && Array.isArray(parsed.events)) saved = parsed;
      } catch { /* Geçersiz JSON: sentetik başlangıç verisine dön. */ }
    }
  }
  const chats = new Map((saved?.chats ?? opts.chats ?? []).map((c) => [c.id, { ...c }]));
  const events = [...(saved?.events ?? opts.events ?? [])];
  const persist = () => {
    if (opts.persistenceKey) {
      localStorage.setItem(opts.persistenceKey, JSON.stringify({ version: 1, chats: [...chats.values()], events }));
    }
  };
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
    if (event.kind === "approval_resolved" && typeof event.payload === "object" && event.payload !== null) {
      const payload = event.payload as { approval_id?: string; status?: ApprovalStatus };
      const approval = payload.approval_id ? approvals.get(payload.approval_id) : undefined;
      if (approval && payload.status) approval.status = payload.status;
    }
    events.push(event);
    trackTurn(event);
    persist();
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
    // Yeniden yüklemede saklanmış olayları zamanlayıcıdan ikinci kez yayınlama.
    if (stream && saved) {
      while (streamIndex < stream.events.length && events.some((event) => event.id === stream.events[streamIndex]!.id)) streamIndex++;
    }
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
    addApproval: (approval) => {
      approvals.set(approval.id, structuredClone(approval));
      append(approval.chat_id ?? "", "approval_requested", { approval_id: approval.id, category: approval.category, reason: approval.reason, summary: approval.action_json });
    },
    listApprovals: (args = {}) => run("list_approvals", [args], () => {
      const all = [...approvals.values()].filter((item) => !args.status || item.status === args.status)
        .sort((a, b) => a.id.localeCompare(b.id));
      const start = args.cursor ? all.findIndex((item) => item.id > args.cursor!) : 0;
      const page = start < 0 ? [] : all.slice(start, start + Math.max(1, Math.min(32, args.limit ?? 32)));
      return { approvals: structuredClone(page), next_cursor: start >= 0 && start + page.length < all.length ? page.at(-1)!.id : null };
    }),
    resolveApproval: (id, approve) => run("resolve_approval", [id, approve], () => {
      const item = approvals.get(id);
      if (!item) throw { kind: "daemon", message: "Onay bulunamadı." } satisfies BridgeError;
      if (item.status !== "pending") throw { kind: "daemon", message: "Onay zaten çözüldü." } satisfies BridgeError;
      if (Date.now() >= Date.parse(item.created_at) + 300_000) {
        item.status = "expired";
        append(item.chat_id ?? "", "approval_resolved", { approval_id: id, status: "expired", by: "timeout" });
        throw { kind: "daemon", message: "Onay süresi doldu." } satisfies BridgeError;
      }
      item.status = approve ? "approved" : "denied";
      item.resolved_at = new Date().toISOString();
      item.resolved_by = "approver";
      append(item.chat_id ?? "", "approval_resolved", { approval_id: id, status: item.status, by: "approver" });
      return { approval: structuredClone(item) };
    }),
    getPolicyConfig: () => run("get_policy_config", [], () => structuredClone(policySettings)),
    setPolicyConfig: (settings) => run("set_policy_config", [settings], () => {
      if (!Number.isInteger(settings.trash_retention_days) || settings.trash_retention_days < 1 || settings.trash_retention_days > 3650) {
        throw { kind: "daemon", message: "Saklama süresi 1–3650 gün olmalı." } satisfies BridgeError;
      }
      if (JSON.stringify(settings.config.deploy_patterns) !== JSON.stringify(policySettings.config.deploy_patterns) || JSON.stringify(settings.config.allow_list) !== JSON.stringify(policySettings.config.allow_list)) {
        throw { kind: "daemon", message: "Salt okunur kurallar değişmiş; ayarları yeniden yükle." } satisfies BridgeError;
      }
      policySettings = structuredClone(settings);
      return structuredClone(policySettings);
    }),
    telegramStatus: () => run("telegram_status", [], () => telegramStatus ? structuredClone(telegramStatus) : reject("Telegram durumu")),
    getTelegramSettings: () => run("get_telegram_settings", [], () => telegramStatus ? structuredClone(telegramStatus.settings) : reject("Telegram ayarları")),
    setTelegramSettings: (settings) => run("set_telegram_settings", [settings], () => {
      if (!telegramStatus) return reject("Telegram ayarları");
      telegramStatus.settings = structuredClone(settings);
      return structuredClone(telegramStatus.settings);
    }),
    telegramTest: () => run("telegram_test", [], () => opts.telegramTestReport ? structuredClone(opts.telegramTestReport) : reject("Telegram testi")),
    listTrash: () => run("list_trash", [], () => ({ entries: structuredClone([...trash.values()]) })),
    restoreTrash: (id) => run("restore_trash", [id], () => {
      const item = trash.get(id);
      if (!item) throw { kind: "daemon", message: "Çöp kaydı bulunamadı." } satisfies BridgeError;
      trash.delete(id);
      return { path: item.original_path };
    }),
    purgeTrash: (id) => run("purge_trash", [id], () => {
      if (!trash.delete(id)) throw { kind: "daemon", message: "Çöp kaydı bulunamadı." } satisfies BridgeError;
      return { id };
    }),
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
        persist();
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
        const page = all.slice(0, Math.max(1, Math.min(1000, limit)));
        return {
          events: page,
          next_after_seq: page.length ? page[page.length - 1]!.seq : after,
          has_more: all.length > page.length,
        };
      }),
    historySearch: (query, id, limit) =>
      run("history_search", [query, id, limit], () => ({
        hits: (opts.searchHits ?? []).filter((h) =>
          (id === undefined || h.chat_id === id) && h.text.toLowerCase().includes(query.toLowerCase()),
        ).slice(0, Math.max(1, Math.min(limit ?? 20, 100))),
      })),
    getQuota: () => run("get_quota", [], () => quota ? structuredClone(quota) : reject("kota")),
    setResetRights: (pool, count) =>
      run("set_reset_rights", [pool, count], () => {
        if (!["claude", "codex"].includes(pool) || !Number.isInteger(count) || count < 0 || count > 4_294_967_295) {
          throw { kind: "daemon", message: "geçersiz sıfırlama hakkı" } satisfies BridgeError;
        }
        const target = quota?.pools.find((item) => item.pool === pool);
        if (!target) return reject("kota");
        target.reset_rights = count;
        return { pool, count };
      }),
    routeMessage: (text) =>
      run("route_message", [text], () => (opts.route ? opts.route(text) : reject("yönlendirme"))),
    // Sahte köprü: gerçek dosya okumaz; boş rapor, gerçek servisle aynı biçimde.
    importCatalog: (path) => run("import_catalog", [path], () => ({ ...(opts.catalogImportReport ?? { imported: 0, updated: 0 }) })),
    importProjects: () =>
      run("import_projects", [], () => ({ ...(opts.projectImportReport ?? { dry_run: false, rows: [], created: 0, notes_updated: 0, summarizer: "null", unmatched_sessions: 0 }) })),
    status: () =>
      run("status", [], () => ({ version: "test", pid: 0, running_turns: activeChats.size, default_engine: opts.defaultEngine ?? "claude" })),
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
