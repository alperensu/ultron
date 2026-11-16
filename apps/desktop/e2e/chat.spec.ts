import { expect, test } from "@playwright/test";
import type { FakeBridgeOptions, StoredEvent } from "../src/lib/bridge";
import { installFakeBridge } from "./fakeData";

function event(seq: number, kind: string, payload: Record<string, unknown>): StoredEvent {
  return {
    id: `chat-event-${seq}`, chat_id: "chat-example", session_id: "session-example",
    seq, ts: new Date(Date.UTC(2026, 0, 1, 12, 0, seq)).toISOString(), kind, payload: { type: kind, ...payload },
  };
}

function chatData(events: StoredEvent[]): FakeBridgeOptions {
  return {
    chats: [{
      id: "chat-example", title: "Örnek: sohbet ekranı", project_id: null, engine: "claude",
      pinned: false, archived: false, updated_at: "2026-01-01T12:00:00Z",
    }],
    events,
  };
}

const exampleEvents = () => [
  event(1, "user_text", { text: "Örnek projedeki testleri incele ve sonucu açıkla." }),
  event(2, "assistant_text", { text: "Testleri ve ilgili dosyaları inceliyorum." }),
  event(3, "tool_call", { tool: "Read", input: {} }),
  event(4, "command", { command: "npm test", exit_code: 0, output_excerpt: "Örnek testler başarılı" }),
  event(5, "file_changed", { path: "src/example.ts", change: "modified" }),
  event(6, "assistant_text", { text: '**İnceleme tamamlandı.**\n\n- Testler başarılı.\n- Dosya değişikliği sağ panelde.\n\n```ts\nconst sonuç = "başarılı";\n```\n\n[Belge](https://example.com/docs)' }),
  event(7, "turn_completed", {}),
];

test("chat_streams_5000_events_smoothly", async ({ page }, testInfo) => {
  test.setTimeout(30_000);
  const history = Array.from({ length: 5000 }, (_, index) => event(index + 1, index % 2 === 0 ? "user_text" : "assistant_text", { text: `Örnek geçmiş mesajı ${index + 1}.` }));
  const live = Array.from({ length: 250 }, (_, index) => event(5001 + index, "assistant_text", { text: ` akış-${index}` }));
  await installFakeBridge(page, { ...chatData(history), stream: { events: live, eventsPerSecond: 50, delayMs: 1800 } });
  await page.goto("/chat/chat-example");
  await expect(page.locator(".chat-screen")).toHaveAttribute("data-event-count", "5000");
  await expect(page.getByRole("feed").getByRole("article")).not.toHaveCount(5000);
  expect(await page.getByRole("feed").getByRole("article").count()).toBeLessThan(60);

  // Ölçüm gerçek requestAnimationFrame zaman damgalarını kullanır; geciken kareler elenmez.
  const deltas = page.evaluate(() => new Promise<number[]>((resolve) => {
    const samples: number[] = [];
    let last = 0;
    let start = 0;
    function frame(now: number) {
      if (!start) start = now;
      if (last) samples.push(now - last);
      last = now;
      if (now - start >= 6500) resolve(samples);
      else requestAnimationFrame(frame);
    }
    requestAnimationFrame(frame);
  }));

  const scroll = page.getByTestId("chat-scroll");
  await scroll.evaluate((element) => { element.scrollTop -= 600; });
  await expect(page.getByRole("button", { name: "yeni mesajlar ↓" })).toBeVisible({ timeout: 5000 });
  const anchor = await scroll.evaluate((element) => element.scrollTop);
  await page.waitForTimeout(350);
  expect(Math.abs(await scroll.evaluate((element) => element.scrollTop) - anchor)).toBeLessThan(2);
  await page.getByRole("button", { name: "yeni mesajlar ↓" }).click();
  await expect(page.getByRole("button", { name: "yeni mesajlar ↓" })).toHaveCount(0);
  await expect(page.locator(".chat-screen")).toHaveAttribute("data-event-count", "5250", { timeout: 9000 });
  expect(await page.getByRole("feed").getByRole("article").count()).toBeLessThan(60);

  const samples = (await deltas).sort((a, b) => a - b);
  const p95 = samples[Math.ceil(samples.length * 0.95) - 1]!;
  console.info(`[chat_streams_5000_events_smoothly] frame-time p95: ${p95.toFixed(2)} ms (${samples.length} samples)`);
  await testInfo.attach("frame-times", { body: JSON.stringify({ count: samples.length, p95, deltas: samples }), contentType: "application/json" });
  expect(samples.length).toBeGreaterThan(120);
  expect(p95).toBeLessThan(20);
});

test("engine_switch_shows_system_line", async ({ page }) => {
  await installFakeBridge(page, chatData(exampleEvents()));
  await page.goto("/chat/chat-example");
  await page.getByRole("button", { name: "Motor: Claude" }).click();
  await expect(page.getByRole("dialog", { name: "Motor seç", exact: true })).toBeInViewport({ ratio: 1 });
  await page.getByRole("button", { name: "Codex", exact: true }).click();
  await expect(page.getByRole("dialog", { name: "Motor değiştirilsin mi?" })).toBeVisible();
  await expect(page.getByText("Motor değişti:")).toHaveCount(0);
  await page.getByRole("button", { name: "Motoru değiştir", exact: true }).click();
  await expect(page.getByText("Motor değişti: Claude → Codex · isteğinle")).toBeVisible();
  await expect(page.getByRole("button", { name: "Motor: Codex" })).toBeVisible();
});

test("chat_screen_screenshot_and_changes", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await installFakeBridge(page, chatData(exampleEvents()));
  await page.goto("/chat/chat-example");
  await expect(page.getByText("İnceleme tamamlandı.")).toBeVisible();
  await page.getByRole("tab", { name: "Değişiklikler" }).click();
  await expect(page.getByRole("tabpanel").getByText("src/example.ts")).toBeVisible();
  await page.getByTestId("chat-scroll").evaluate((element) => { element.scrollTop = 0; });
  await page.waitForTimeout(500);
  await page.screenshot({ path: "e2e/__screenshots__/chat-1280x800.png" });
});

test("chat_cancel_and_current_chat_composer", async ({ page }) => {
  await installFakeBridge(page, chatData([event(1, "user_text", { text: "Örnek çalışan tur" })]));
  await page.goto("/chat/chat-example");
  await expect(page.getByRole("button", { name: "Gönder", exact: true })).toBeDisabled();
  await page.getByRole("button", { name: "Durdur", exact: true }).click();
  await expect(page.getByRole("button", { name: "Durdur", exact: true })).toHaveCount(0);
  await page.getByRole("textbox", { name: "Mesaj", exact: true }).fill("Aynı sohbete devam et");
  await page.getByRole("button", { name: "Gönder", exact: true }).click();
  await expect(page).toHaveURL(/\/chat\/chat-example$/);
  await expect(page.getByText("Aynı sohbete devam et", { exact: true })).toBeVisible();
});
