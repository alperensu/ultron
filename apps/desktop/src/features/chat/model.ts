import type { ApprovalStatus, EngineId, StoredEvent } from "../../lib/bridge";
import { ENGINE_LABEL } from "../../lib/format";
import { riskLabel } from "../approvals/labels";

export type ChatRow =
  | { id: string; type: "user" | "assistant"; text: string }
  | { id: string; type: "work"; events: StoredEvent[]; startedAt: string; finishedAt: string }
  | { id: string; type: "approval"; approvalId: string; status: ApprovalStatus }
  | { id: string; type: "system"; tone: "engine" | "quota" | "error" | "policy"; text: string };

export interface ChatSnapshot {
  rows: ChatRow[];
  files: StoredEvent[];
  /** Task 11 bu olayları ajan oturumlarına dönüştürebilir. */
  events: StoredEvent[];
  running: boolean;
  engine: EngineId | null;
  version: number;
}

export function eventPayload(event: StoredEvent): Record<string, unknown> {
  return typeof event.payload === "object" && event.payload !== null
    ? event.payload as Record<string, unknown>
    : {};
}

export function field(event: StoredEvent, name: string): string {
  const value = eventPayload(event)[name];
  return typeof value === "string" ? value : "";
}

export function engineId(value: unknown): EngineId | null {
  return value === "claude" || value === "codex" || value === "agy" ? value : null;
}

const WORK_KINDS = new Set(["tool_call", "tool_result", "command", "file_changed", "external_changes"]);

export function workEventSummary(event: StoredEvent): string {
  const payload = eventPayload(event);
  switch (event.kind) {
    case "command":
      return `${field(event, "command")} · çıkış: ${typeof payload.exit_code === "number" ? payload.exit_code : "bekleniyor"}`;
    case "file_changed":
      return `${field(event, "path")} · ${field(event, "change")}`;
    case "tool_call":
      return `${field(event, "tool")} · çağrıldı`;
    case "tool_result":
      return `${field(event, "tool")} · ${payload.is_error ? "hata" : "tamamlandı"}: ${field(event, "output_excerpt")}`;
    default:
      return field(event, "summary");
  }
}

/** Yalnız yeni olaylar işlenir. React'e verilen önceki kare hiçbir zaman değiştirilmez. */
export function createChatTimeline(initialEngine: EngineId | null = null) {
  let snapshot: ChatSnapshot = {
    rows: [], files: [], events: [], running: false, engine: initialEngine, version: 0,
  };
  const seen = new Set<number>();
  let previousKind = "";

  function append(incoming: StoredEvent[]): ChatSnapshot {
    const fresh = incoming.filter((event) => !seen.has(event.seq)).sort((a, b) => a.seq - b.seq);
    if (fresh.length === 0) return snapshot;
    const rows = [...snapshot.rows];
    const files = [...snapshot.files];
    const events = [...snapshot.events];
    let { running, engine } = snapshot;
    for (const event of fresh) {
      if (seen.has(event.seq)) continue;
      seen.add(event.seq);
      events.push(event);
      const tail = rows[rows.length - 1];
      if (event.kind === "assistant_text") {
        const text = field(event, "text");
        if (previousKind === "assistant_text" && tail?.type === "assistant") {
          rows[rows.length - 1] = { ...tail, text: tail.text + text };
        } else {
          rows.push({ id: event.id, type: "assistant", text });
        }
        running = true;
      } else if (event.kind === "user_text") {
        rows.push({ id: event.id, type: "user", text: field(event, "text") });
        running = true;
      } else if (WORK_KINDS.has(event.kind)) {
        if (tail?.type === "work") {
          rows[rows.length - 1] = { ...tail, events: [...tail.events, event], finishedAt: event.ts };
        } else {
          rows.push({ id: event.id, type: "work", events: [event], startedAt: event.ts, finishedAt: event.ts });
        }
        if (event.kind === "file_changed") files.push(event);
      } else if (event.kind === "engine_switched") {
        const from = engineId(eventPayload(event).from);
        const to = engineId(eventPayload(event).to);
        if (to) engine = to;
        const reason = field(event, "reason");
        rows.push({
          id: event.id, type: "system", tone: "engine",
          text: `Motor değişti: ${from ? ENGINE_LABEL[from] : "Önceki motor"} → ${to ? ENGINE_LABEL[to] : "Bilinmiyor"}${reason === "quota" ? " · kota nedeniyle" : reason === "error" ? " · hata nedeniyle" : " · isteğinle"}`,
        });
      } else if (event.kind === "approval_requested") {
        rows.push({ id: event.id, type: "approval", approvalId: field(event, "approval_id"), status: "pending" });
      } else if (event.kind === "policy_notice") {
        // Hata değil: ham silme yerine `ultron trash` yönlendirmesi; tur sürüyor olabilir.
        rows.push({ id: event.id, type: "system", tone: "policy", text: field(event, "message") });
      } else if (event.kind === "policy_violation") {
        rows.push({ id: event.id, type: "system", tone: "policy",
          text: `Güvenlik kuralı ihlali · ${riskLabel(field(event, "category"))}\nGerçekleşmiş riskli işlem: ${field(event, "command")}\nBu bildirim işlemi geri almaz. Sonucu kontrol et.` });
      } else if (event.kind === "approval_resolved") {
        const status = field(event, "status");
        const existing = rows.findIndex((row) => row.type === "approval" && row.approvalId === field(event, "approval_id"));
        const row = rows[existing];
        if (row?.type === "approval" && (status === "approved" || status === "denied" || status === "expired")) {
          rows[existing] = { ...row, status };
        } else rows.push({ id: event.id, type: "system", tone: "policy", text: status === "approved" ? "İşlem onaylandı." : status === "denied" ? "İşlem reddedildi." : "Onayın süresi doldu." });
      } else if (event.kind === "quota_exhausted" || event.kind === "error") {
        rows.push({
          id: event.id, type: "system", tone: event.kind === "error" ? "error" : "quota",
          text: `${event.kind === "error" ? "Hata" : "Kota doldu"}: ${field(event, "message")}`,
        });
        running = false;
      } else if (event.kind === "turn_completed") {
        running = false;
      } else if (event.kind === "session_started") {
        running = true;
      }
      previousKind = event.kind;
    }
    snapshot = { rows, files, events, running, engine, version: events.length };
    return snapshot;
  }

  return { append, getSnapshot: () => snapshot };
}
