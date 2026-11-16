import type { EngineId, StoredEvent } from "../../lib/bridge";
import { ENGINE_LABEL } from "../../lib/format";
import { engineId, eventPayload, field, workEventSummary } from "../chat/model";

export type AgentStatus = "running" | "completed" | "error" | "quota" | "closed" | "unknown";
export interface Agent {
  id: string;
  sessionId: string;
  parentId?: string;
  engine: EngineId | null;
  title: string;
  status: AgentStatus;
  startedAt: string;
  finishedAt: string | null;
  step: string;
  events: StoredEvent[];
}

export const STATUS_LABEL: Record<AgentStatus, string> = {
  running: "Çalışıyor", completed: "Tamamlandı", error: "Hata", quota: "Kota doldu", closed: "Kapandı", unknown: "Durum bilinmiyor",
};

export function eventSummary(event: StoredEvent): string {
  const text = field(event, "text") || field(event, "message") || workEventSummary(event);
  if (text) return text.replace(/\s+/g, " ").trim();
  switch (event.kind) {
    case "session_started": return "Oturum başladı";
    case "turn_completed": return "Tur tamamlandı";
    case "engine_switched": return "Motor devredildi";
    case "usage": return "Kullanım bilgisi güncellendi";
    default: return "Olay alındı";
  }
}

/** Şema çağrı kimliği taşımıyorsa sonuçlar aynı oturum/araç içinde FIFO eşleşir. */
export function deriveAgents(events: StoredEvent[], chatId: string, lastEngine: EngineId | null): Agent[] {
  const ordered = events.filter((event) => event.chat_id === chatId).slice().sort((a, b) => a.seq - b.seq);
  const firstSwitch = ordered.find((event) => event.kind === "engine_switched");
  let engine = firstSwitch ? engineId(eventPayload(firstSwitch).from) : lastEngine;
  const sessions = new Map<string, Agent>();
  const agents: Agent[] = [];
  const pending = new Map<string, Agent[]>();
  const seen = new Set<number>();
  for (const event of ordered) {
    if (seen.has(event.seq)) continue;
    seen.add(event.seq);
    const sessionId = event.session_id;
    let session = sessionId ? sessions.get(sessionId) : undefined;
    if (sessionId && !session) {
      const explicit = engineId(eventPayload(event).engine);
      const sessionEngine = explicit ?? engine;
      session = {
        id: `session:${sessionId}`, sessionId, engine: sessionEngine,
        title: `${sessionEngine ? ENGINE_LABEL[sessionEngine] : "Motor bilinmiyor"} oturumu`,
        status: "unknown", startedAt: event.ts, finishedAt: null, step: "", events: [],
      };
      sessions.set(sessionId, session);
      agents.push(session);
    }
    if (session) {
      session.events.push(event);
      session.step = eventSummary(event);
      if (["session_started", "user_text", "assistant_text", "tool_call", "command"].includes(event.kind)) {
        session.status = "running";
        session.finishedAt = null;
      }
      const terminal: Partial<Record<string, AgentStatus>> = {
        turn_completed: "completed", error: "error", quota_exhausted: "quota", engine_switched: "closed",
      };
      if (terminal[event.kind]) {
        session.status = terminal[event.kind]!;
        session.finishedAt = event.ts;
        for (const child of agents.filter((agent) => agent.parentId === session!.id && agent.status === "running")) {
          child.status = "unknown";
          child.finishedAt = event.ts;
          child.step = "Alt ajan sonucu alınmadı";
        }
      }
      const tool = field(event, "tool");
      if (session.engine === "claude" && (tool === "Task" || tool === "Agent")) {
        const key = `${session.sessionId}:${tool}`;
        if (event.kind === "tool_call") {
          const input = eventPayload(event).input;
          const description = typeof input === "object" && input !== null && "description" in input && typeof input.description === "string" ? input.description : "Alt ajan";
          const child: Agent = {
            id: `tool:${event.id}`, parentId: session.id, sessionId: session.sessionId, engine: "claude",
            title: description, status: "running", startedAt: event.ts, finishedAt: null,
            step: description, events: [event],
          };
          agents.push(child);
          pending.set(key, [...(pending.get(key) ?? []), child]);
        } else if (event.kind === "tool_result") {
          const child = pending.get(key)?.shift();
          if (child) {
            child.events.push(event);
            child.status = eventPayload(event).is_error === true ? "error" : "completed";
            child.finishedAt = event.ts;
            child.step = field(event, "output_excerpt") || STATUS_LABEL[child.status];
          }
        }
      }
    }
    if (event.kind === "engine_switched") engine = engineId(eventPayload(event).to);
  }
  return agents;
}

export function elapsed(agent: Agent, now: number): string {
  const end = agent.finishedAt ? Date.parse(agent.finishedAt) : now;
  const seconds = Math.max(0, Math.floor((end - Date.parse(agent.startedAt)) / 1000));
  if (!Number.isFinite(seconds)) return "—";
  return `${Math.floor(seconds / 60).toString().padStart(2, "0")}:${(seconds % 60).toString().padStart(2, "0")}`;
}
