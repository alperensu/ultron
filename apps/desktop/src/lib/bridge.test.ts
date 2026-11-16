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
    const b = createFakeBridge({ chats: [chat] });
    const detail = await b.getChat("c1");
    expect(sorted(detail)).toEqual(["chat", "last_engine", "work_state"]);
    expect(sorted(detail.chat)).toEqual(sorted(chat));
    expect(sorted(await b.importCatalog("x.md"))).toEqual(["imported", "updated"]);
    expect(sorted(await b.importClaudeHistory())).toEqual(["chats", "files", "masked", "messages", "skipped"]);
    expect(sorted(await b.status())).toEqual(["default_engine", "pid", "running_turns", "version"]);
  });
});
