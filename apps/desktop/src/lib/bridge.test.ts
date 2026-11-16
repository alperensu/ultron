import { describe, expect, it } from "vitest";
import {
  createFakeBridge,
  describeBridgeError,
  isDaemonUnavailable,
  mergeEvents,
  payloadOf,
  toBridgeError,
  type StoredEvent,
} from "./bridge";

const ev = (seq: number, kind = "assistant_text", chat = "c1"): StoredEvent => ({
  id: `e${seq}`,
  chat_id: chat,
  session_id: null,
  seq,
  ts: "2026-01-01T00:00:00Z",
  kind,
  payload: { type: kind, text: `m${seq}` },
});

describe("bridge tip yardımcıları", () => {
  it("toBridgeError tipli hatayı korur, diğerlerini protocol yapar", () => {
    const typed = { kind: "daemon", message: "henüz uygulanmadı (Aşama 2 Task 5)" } as const;
    expect(toBridgeError(typed)).toBe(typed);
    expect(toBridgeError("boom")).toEqual({ kind: "protocol", message: "boom" });
    expect(toBridgeError(new Error("x"))).toEqual({ kind: "protocol", message: "x" });
  });

  it("isDaemonUnavailable ve describeBridgeError", () => {
    const e = { kind: "daemon_unavailable", reason: "yok" } as const;
    expect(isDaemonUnavailable(e)).toBe(true);
    expect(isDaemonUnavailable({ kind: "daemon", message: "m" })).toBe(false);
    expect(describeBridgeError(e)).toContain("yok");
    expect(describeBridgeError({ kind: "daemon", message: "aynen" })).toBe("aynen");
  });

  it("payloadOf yalnız eşleşen türde döner", () => {
    expect(payloadOf<{ text: string }>(ev(1), "assistant_text")?.text).toBe("m1");
    expect(payloadOf(ev(1), "command")).toBeNull();
  });

  it("mergeEvents tekrarları eler ve sırayı korur", () => {
    const merged = mergeEvents([ev(1), ev(2)], [ev(2), ev(4), ev(3)]);
    expect(merged.map((e) => e.seq)).toEqual([1, 2, 3, 4]);
  });
});

describe("sahte köprü", () => {
  it("olay yayar ve sayfalar", async () => {
    const b = createFakeBridge({ events: [ev(1), ev(2), ev(3)] });
    const got: number[] = [];
    const off = await b.onEvent((e) => got.push(e.seq));
    b.emit(ev(4));
    off();
    b.emit(ev(5));
    expect(got).toEqual([4]);
    const page = await b.eventsPage("c1", 1, 2);
    expect(page.events.map((e) => e.seq)).toEqual([2, 3]);
    expect(page.has_more).toBe(true);
  });

  it("uygulanmamış istekler uydurma veri yerine tipli hata verir; failWith her komutu reddeder", async () => {
    const b = createFakeBridge();
    await expect(b.getQuota()).rejects.toMatchObject({ kind: "daemon" });
    const down = createFakeBridge({ failWith: { kind: "daemon_unavailable", reason: "r" } });
    await expect(down.status()).rejects.toMatchObject({ kind: "daemon_unavailable" });
  });
});

describe("komut JSON sözleşmesi (Rust testindeki alan listeleriyle aynı)", () => {
  const chat = {
    id: "c1",
    title: "t",
    project_id: null,
    pinned: false,
    archived: false,
    updated_at: "2026-01-01T00:00:00Z",
    engine: "claude" as const,
  };
  const sorted = (o: object) => Object.keys(o).sort();

  it("sahte köprü gerçek servisle aynı biçimleri döndürür", async () => {
    const b = createFakeBridge({
      chats: [chat], events: [ev(1), ev(2, "turn_completed")],
      projects: [{ id: "p1", name: "Sentetik", path: "/sentetik", aliases: [], description: null, last_active_at: null }],
      searchHits: [{ chat_id: "c1", event_id: "e1", text: "merhaba" }],
      route: () => ({ project_id: null, confidence: 0, reason: "Sentetik", alternatives: [] }),
      quota: { fetched_at: "2026-01-01T00:00:00Z", pools: [{ pool: "claude", reset_rights: 0, mode: "Dengeli", windows: [{ label: "Sentetik", used_pct: null, resets_at: null, window_secs: 0, source: "Unknown" }] }] },
    });
    const projects = await b.listProjects();
    expect(sorted(projects)).toEqual(["projects"]);
    expect(sorted(projects.projects[0]!)).toEqual(["aliases", "description", "id", "last_active_at", "name", "path"]);
    expect(sorted(await b.listChats())).toEqual(["chats"]);
    expect(sorted((await b.listChats()).chats[0]!)).toEqual(sorted(chat));
    const detail = await b.getChat("c1");
    expect(sorted(detail)).toEqual(["chat", "last_engine", "work_state"]);
    expect(sorted(detail.chat)).toEqual(sorted(chat));
    const updated = await b.updateChat("c1", { title: "Yeni başlık", pinned: true });
    expect(sorted(updated)).toEqual(["chat"]);
    expect(sorted(updated.chat)).toEqual(sorted(chat));
    expect(sorted(await b.switchEngine("c1", "codex"))).toEqual(["chat_id"]);
    const sent = await b.sendMessage({ text: "Sentetik" });
    expect(sorted(sent)).toEqual(["chat_id"]);
    expect(sorted(await b.cancelTurn(sent.chat_id))).toEqual(["chat_id"]);
    const page = await b.eventsPage("c1", null, 1);
    expect(sorted(page)).toEqual(["events", "has_more", "next_after_seq"]);
    expect(sorted(page.events[0]!)).toEqual(["chat_id", "id", "kind", "payload", "seq", "session_id", "ts"]);
    const hits = await b.historySearch("merhaba");
    expect(sorted(hits)).toEqual(["hits"]);
    expect(sorted(hits.hits[0]!)).toEqual(["chat_id", "event_id", "text"]);
    const quota = await b.getQuota();
    expect(sorted(quota)).toEqual(["fetched_at", "pools"]);
    expect(sorted(quota.pools[0]!)).toEqual(["mode", "pool", "reset_rights", "windows"]);
    expect(sorted(quota.pools[0]!.windows[0]!)).toEqual(["label", "resets_at", "source", "used_pct", "window_secs"]);
    expect(await b.setResetRights("claude", 2)).toEqual({ pool: "claude", count: 2 });
    expect(sorted(await b.routeMessage("devam"))).toEqual(["alternatives", "confidence", "project_id", "reason"]);
    expect((await b.getQuota()).pools[0]!.reset_rights).toBe(2);
    expect(sorted(await b.routeMessage("Sentetik"))).toEqual(["alternatives", "confidence", "project_id", "reason"]);
    expect(sorted(await b.importCatalog("x.md"))).toEqual(["imported", "updated"]);
    expect(sorted(await b.importClaudeHistory())).toEqual(["chats", "files", "masked", "messages", "skipped"]);
    expect(sorted(await b.status())).toEqual(["default_engine", "pid", "running_turns", "version"]);
  });

  it("sahte köprü aramayı sohbete göre süzer ve sayfa sınırlarını servis gibi uygular", async () => {
    const b = createFakeBridge({
      events: [ev(1), ev(2)],
      searchHits: [{ chat_id: "c1", event_id: "e1", text: "merhaba" }, { chat_id: "c2", event_id: "e2", text: "merhaba" }],
    });
    expect((await b.eventsPage("c1", null, 0)).events).toHaveLength(1);
    expect((await b.historySearch("merhaba", "c2")).hits.map((hit) => hit.chat_id)).toEqual(["c2"]);
    expect((await b.historySearch("merhaba", undefined, 0)).hits).toHaveLength(1);
  });
});
