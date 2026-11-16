import { expect, test, type Locator, type Page } from "@playwright/test";
import type { FakeBridgeOptions, StoredEvent } from "../src/lib/bridge";
import { installFakeBridge, sampleBridge } from "./fakeData";

// Tüm mesajlar, projeler ve kota değerleri sentetik örnektir; gerçek motor çalışmaz.
const chatId = "sohbet-1";
const prompt = "Örnek: Ultron kabul akışını doğrula";
const storageKey = "ultron:e2e:acceptance:v1";

function event(seq: number, kind: string, payload: Record<string, unknown> = {}): StoredEvent {
  return {
    id: `acceptance-${seq}`, chat_id: chatId, session_id: "acceptance-session", seq,
    ts: new Date(Date.now() - (10 - seq) * 1000).toISOString(), kind,
    payload: { type: kind, ...payload },
  };
}

function screenshotData(): FakeBridgeOptions {
  return {
    ...sampleBridge(),
    chats: [{ id: chatId, title: prompt, project_id: "p1", engine: "claude", pinned: false, archived: false, updated_at: new Date().toISOString() }],
    events: [
      event(1, "user_text", { text: prompt }),
      event(2, "session_started"),
      event(3, "assistant_text", { text: "Örnek: kabul testlerinin sonuçlarını inceliyorum." }),
      event(4, "command", { command: "npm test", exit_code: 0, output_excerpt: "Sentetik örnek test sonucu" }),
      event(5, "assistant_text", { text: "**Örnek yanıt:** sohbet ve olay günlüğü hazır." }),
      { ...event(6, "turn_completed"), ts: new Date().toISOString() },
    ],
  };
}

async function agentDetail(page: Page): Promise<Locator> {
  const panel = page.getByRole("complementary", { name: "Sohbet ayrıntıları", exact: true });
  await panel.getByRole("button", { name: /Claude oturumu/ }).click();
  const detail = panel.getByRole("region", { name: "Claude oturumu", exact: true });
  await expect(detail).toBeVisible();
  await expect(detail).toHaveCSS("opacity", "1");
  return detail;
}

test("acceptance: Today → yönlendirme → canlı tur → motor devri → yeniden yükleme", async ({ page }) => {
  await installFakeBridge(page, { ...sampleBridge(), chats: [], events: [], persistenceKey: storageKey });
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Bugün", level: 1 })).toBeVisible();
  await page.getByRole("textbox", { name: "Mesaj", exact: true }).fill(prompt);
  await expect(page.getByRole("button", { name: /Proje: → Ultron \(otomatik\)/ })).toBeVisible();

  // İlk AppStore aboneliği kurulmuş durumda. Akış, gönderimden sonra sohbetin
  // useChat aboneliği açıldığında başlar; makine hızına bağlı bir başlangıç gecikmesi yoktur.
  await page.evaluate((events) => {
    window.__ULTRON_FAKE_BRIDGE__!.stream = { events, delayMs: 250, eventsPerSecond: 2 };
  }, [
    event(2, "session_started"),
    event(3, "assistant_text", { text: "Örnek yanıt akıyor." }),
    event(4, "assistant_text", { text: " Kabul turu tamamlandı." }),
    event(5, "turn_completed"),
  ]);
  await page.getByRole("button", { name: "Gönder", exact: true }).click();
  await expect(page).toHaveURL(/\/chat\/sohbet-1$/);
  const feed = page.getByRole("feed");
  await expect(feed.getByText(prompt, { exact: true })).toBeVisible();
  await expect(feed.getByText("Örnek yanıt akıyor.", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Motor: Claude", exact: true })).toBeDisabled();
  await expect(feed.getByText("Örnek yanıt akıyor. Kabul turu tamamlandı.", { exact: true })).toBeVisible();
  await expect(page.locator(".chat-screen")).toHaveAttribute("data-event-count", "5");
  await expect(page.getByRole("button", { name: "Motor: Claude", exact: true })).toBeEnabled();

  await page.getByRole("button", { name: "Motor: Claude", exact: true }).click();
  await page.getByRole("button", { name: "Codex", exact: true }).click();
  await expect(page.getByRole("dialog", { name: "Motor değiştirilsin mi?" })).toBeVisible();
  await page.getByRole("button", { name: "Motoru değiştir", exact: true }).click();
  const systemLine = "Motor değişti: Claude → Codex · isteğinle";
  await expect(feed.getByText(systemLine, { exact: true })).toBeVisible();
  await expect(page.locator(".chat-screen")).toHaveAttribute("data-event-count", "6");
  const saved = await page.evaluate((key) => localStorage.getItem(key), storageKey);
  expect(saved).not.toBeNull();

  await page.reload();
  await expect(page.locator(".chat-screen")).toHaveAttribute("data-event-count", "6");
  await expect(feed.getByText(prompt, { exact: true })).toBeVisible();
  await expect(feed.getByText("Örnek yanıt akıyor. Kabul turu tamamlandı.", { exact: true })).toBeVisible();
  await expect(feed.getByText(systemLine, { exact: true })).toHaveCount(1);
  await expect(page.getByRole("button", { name: "Motor: Codex", exact: true })).toBeVisible();
  // Açık sohbetin proje grubu otomatik genişler; sohbet listede de erişilebilir olmalı.
  const sidebar = page.getByRole("complementary", { name: "Kenar çubuğu", exact: true });
  await expect(sidebar.locator(".project-row", { hasText: "Ultron" })).toHaveAttribute("aria-expanded", "true");
  await expect(sidebar.getByRole("button", { name: prompt, exact: true })).toBeVisible();
  expect(await page.evaluate((key) => localStorage.getItem(key), storageKey)).toBe(saved);
});

test("acceptance: kalıcı sahte akış yeniden yüklemede olayları tekrarlamaz", async ({ page }) => {
  const data = screenshotData();
  await installFakeBridge(page, {
    ...data, events: data.events!.slice(0, -1), persistenceKey: storageKey,
    stream: { events: [data.events!.at(-1)!], delayMs: 100 },
  });
  await page.goto(`/chat/${chatId}`);
  await expect(page.locator(".chat-screen")).toHaveAttribute("data-event-count", "6");
  const saved = await page.evaluate((key) => localStorage.getItem(key), storageKey);
  await page.reload();
  await expect(page.locator(".chat-screen")).toHaveAttribute("data-event-count", "6");
  // Aynı yayın penceresini geçir; yeni bir kayıt yazılması/olay çoğalması hatadır.
  await page.waitForTimeout(300);
  expect(await page.evaluate((key) => localStorage.getItem(key), storageKey)).toBe(saved);
});

for (const [width, height] of [[1280, 800], [1920, 1080]] as const) {
  test(`acceptance: ekran görüntüleri ${width}x${height}`, async ({ page }) => {
    await page.setViewportSize({ width, height });
    await installFakeBridge(page, screenshotData());
    const screenshot = async (screen: string) => {
      await page.evaluate(() => document.fonts.ready.then(() => undefined));
      await page.screenshot({ path: `e2e/__screenshots__/acceptance-${screen}-${width}x${height}.png`, animations: "disabled" });
    };
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Bugün", level: 1 })).toBeVisible();
    await expect(page.getByText("Son 24 saatte 1 tur bitti.")).toBeVisible();
    await screenshot("today");

    await page.goto(`/chat/${chatId}`);
    await expect(page.getByRole("feed").getByText(/sohbet ve olay günlüğü hazır/)).toBeVisible();
    await screenshot("chat");
    const detail = await agentDetail(page);
    await screenshot("agent-detail");
    await detail.getByRole("button", { name: "‹ Ajanlar" }).click();

    await page.getByRole("button", { name: "Kota panelini aç", exact: true }).click();
    const sheet = page.getByRole("dialog", { name: "Kota", exact: true });
    await expect(sheet).toBeInViewport({ ratio: 1 });
    await expect(sheet).toHaveCSS("opacity", "1");
    await screenshot("quota");
  });
}

test("acceptance: reduced motion sheet ve panel geçişlerini sıfırlar", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await installFakeBridge(page, screenshotData());
  await page.goto(`/chat/${chatId}`);
  const detail = await agentDetail(page);
  const expectReducedTransition = async (element: typeof detail) => {
    await expect.poll(async () => element.evaluate((node) =>
      getComputedStyle(node).transitionDuration.split(",").every((duration) => {
        const value = duration.trim();
        const seconds = Number.parseFloat(value) / (value.endsWith("ms") ? 1000 : 1);
        return seconds <= 0.001;
      }),
    )).toBe(true);
  };
  await expectReducedTransition(detail);
  await expect(detail).toHaveCSS("transform", "none");
  await page.getByRole("button", { name: "Kota panelini aç", exact: true }).click();
  const sheet = page.getByRole("dialog", { name: "Kota", exact: true });
  await expect(sheet).toBeVisible();
  await expectReducedTransition(sheet);
  await expectReducedTransition(page.getByTestId("sheet-scrim"));
});
