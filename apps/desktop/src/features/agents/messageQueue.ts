import { describeBridgeError, type Bridge } from "../../lib/bridge";
import { afterTurn } from "./afterTurn";

export interface QueuedMessage {
  id: number;
  text: string;
  status: "queued" | "sending" | "sent" | "failed";
  error?: string;
}

/** Sohbet başına tek kuyruk; ayrıntı/listenin kapanması bekleyen mesajı kaybetmez. */
export function createAgentMessageQueue(bridge: Bridge, chatId: string) {
  let rows: QueuedMessage[] = [];
  let nextId = 0;
  let sending = false;
  let active = false;
  let permit = false;
  let ready: Promise<void> | undefined;
  let off: (() => void) | undefined;
  let connections = 0;
  let activitySeq = -1;
  const listeners = new Set<() => void>();
  let lastCompletion = -1;
  const publish = () => listeners.forEach((listener) => listener());
  const update = (id: number, patch: Partial<QueuedMessage>) => {
    rows = rows.map((row) => row.id === id ? { ...row, ...patch } : row);
    publish();
  };
  function stopIfIdle() {
    if (connections > 0) return;
    if (rows.some((row) => row.status === "queued" || row.status === "sending")) return;
    off?.();
    off = undefined;
    ready = undefined;
    active = false;
  }

  async function flush() {
    if (sending || active || !permit) return;
    const row = rows.find((item) => item.status === "queued");
    if (!row) return;
    permit = false;
    sending = true;
    update(row.id, { status: "sending" });
    try {
      await afterTurn(() => bridge.sendMessage({ chat_id: chatId, text: row.text }));
      update(row.id, { status: "sent" });
    } catch (error) {
      update(row.id, { status: "failed", error: describeBridgeError(error) });
    } finally {
      sending = false;
      stopIfIdle();
      void flush();
    }
  }

  function start() {
    ready ??= bridge.onEvent((event) => {
      if (event.chat_id !== chatId) return;
      if (["user_text", "session_started", "assistant_text"].includes(event.kind) && event.seq > activitySeq) {
        activitySeq = event.seq;
        active = true;
      }
      if (event.kind !== "turn_completed" || event.seq <= lastCompletion) return;
      lastCompletion = event.seq;
      if (event.seq < activitySeq) return;
      activitySeq = event.seq;
      active = false;
      permit = true;
      void flush();
    }).then((unsubscribe) => { off = unsubscribe; stopIfIdle(); }).catch((error: unknown) => {
      ready = undefined;
      throw error;
    });
    return ready;
  }

  return {
    getSnapshot: () => rows,
    subscribe(listener: () => void) {
      listeners.add(listener);
      return () => { listeners.delete(listener); };
    },
    connect() {
      connections++;
      // Gönderim öncesinde abonelik tekrar denenir ve hatası kullanıcıya gösterilir.
      void start().catch(() => undefined);
      return () => { connections--; stopIfIdle(); };
    },
    async enqueue(text: string, running: boolean, visibleSeq = Infinity) {
      const trimmed = text.trim();
      if (!trimmed) return;
      if (rows.filter((row) => row.status !== "sent").length >= 100) {
        throw new Error("Mesaj kuyruğu dolu. Yeni mesaj eklemek için bir satırı kaldır.");
      }
      const waiting = rows.some((row) => row.status === "queued" || row.status === "sending");
      const observedNewerActivity = activitySeq > visibleSeq;
      if (!waiting && !observedNewerActivity) active = running;
      const id = ++nextId;
      rows = [...rows.filter((row) => row.status !== "sent"), { id, text: trimmed, status: "queued" }];
      // Bir turda en fazla bir mesaj; kalanlar sonraki gerçek tamamlanmayı bekler.
      if (running && !observedNewerActivity) { active = true; permit = false; }
      else if (!sending && !active) permit = true;
      publish();
      try { await start(); }
      catch (error) {
        update(id, { status: "failed", error: describeBridgeError(error) });
        throw error;
      }
      void flush();
    },
    remove(id: number) {
      rows = rows.filter((row) => row.id !== id || row.status === "sending");
      publish();
      stopIfIdle();
    },
    dispose() { off?.(); listeners.clear(); },
  };
}

const queues = new WeakMap<Bridge, Map<string, ReturnType<typeof createAgentMessageQueue>>>();
export function agentMessageQueue(bridge: Bridge, chatId: string) {
  let chats = queues.get(bridge);
  if (!chats) { chats = new Map(); queues.set(bridge, chats); }
  let queue = chats.get(chatId);
  if (!queue) { queue = createAgentMessageQueue(bridge, chatId); chats.set(chatId, queue); }
  return queue;
}
