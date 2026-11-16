import type { Page } from "@playwright/test";
import type { FakeBridgeOptions, StoredEvent } from "../src/lib/bridge";

// Yalnızca e2e: tamamı sentetik örnek veri (gerçek proje, kişi veya yol yok).

const minutesAgo = (m: number) => new Date(Date.now() - m * 60_000).toISOString();

const event = (id: string, chat: string, seq: number, kind: string, ago: number): StoredEvent => ({
  id,
  chat_id: chat,
  session_id: null,
  seq,
  ts: minutesAgo(ago),
  kind,
  payload: { type: kind },
});

export function sampleBridge(): FakeBridgeOptions {
  const projects = [
    { id: "p1", name: "Ultron", path: "/ornek/ultron", aliases: [], description: "Yerel harness", last_active_at: null },
    { id: "p2", name: "Mobil uygulama", path: "/ornek/mobil", aliases: [], description: null, last_active_at: null },
  ];
  const chat = (
    id: string,
    title: string,
    project_id: string | null,
    updatedAgo: number,
    engine: "claude" | "codex",
    pinned = false,
  ) => ({ id, title, project_id, pinned, archived: false, updated_at: minutesAgo(updatedAgo), engine });
  return {
    projects,
    chats: [
      chat("c1", "Masaüstü iskeleti", "p1", 120, "claude"),
      chat("c2", "Kota paneli taslağı", "p1", 300, "codex"),
      chat("c3", "Haftalık plan", null, 600, "claude", true),
      chat("c4", "Eski deneme", null, 4320, "claude"),
      chat("c5", "Giriş ekranı", "p2", 1000, "claude"),
    ],
    events: [
      event("e1", "c1", 1, "user_text", 134),
      event("e2", "c1", 2, "turn_completed", 120),
      event("e3", "c2", 1, "user_text", 305),
      event("e4", "c2", 2, "turn_completed", 300),
      // 24 saatten eski tur listelenmez.
      event("e5", "c4", 1, "user_text", 4330),
      event("e6", "c4", 2, "turn_completed", 4320),
    ],
    quota: {
      fetched_at: minutesAgo(1),
      pools: [
        {
          pool: "claude",
          mode: "normal",
          reset_rights: null,
          windows: [{ label: "5 saat", used_pct: 42, resets_at: null, window_secs: 18000, source: "Live" }],
        },
        {
          pool: "codex",
          mode: "normal",
          reset_rights: null,
          windows: [{ label: "Haftalık", used_pct: 87, resets_at: null, window_secs: 604800, source: "Estimated" }],
        },
        {
          pool: "agy",
          mode: "normal",
          reset_rights: null,
          windows: [{ label: "Günlük", used_pct: null, resets_at: null, window_secs: 86400, source: "Unknown" }],
        },
      ],
    },
    searchHits: [{ chat_id: "c2", event_id: "h1", text: "kota paneli için halka ve çubuk tasarımı" }],
  };
}

/** Sahte köprüyü sayfa yüklenmeden önce yerleştirir; yönlendirme her zaman "Ultron" projesini önerir. */
export async function installFakeBridge(page: Page, data: FakeBridgeOptions = sampleBridge()): Promise<void> {
  await page.addInitScript((payload) => {
    window.__ULTRON_FAKE_BRIDGE__ = {
      ...payload,
      route: () => ({ project_id: "p1", confidence: 0.9, reason: "ad eşleşti", alternatives: [] }),
    };
  }, data);
}
