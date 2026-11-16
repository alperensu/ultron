import { expect, test } from "@playwright/test";
import type { FakeBridgeOptions, StoredEvent } from "../src/lib/bridge";
import { installFakeBridge } from "./fakeData";

function event(seq: number, kind: string, payload: Record<string, unknown> = {}): StoredEvent {
  return {
    id: `example-agent-${seq}`, chat_id: "chat-agents", session_id: "session-agents", seq,
    ts: new Date(Date.now() - (10 - seq) * 1000).toISOString(), kind, payload: { type: kind, ...payload },
  };
}
function data(): FakeBridgeOptions {
  return {
    chats: [{ id: "chat-agents", title: "Örnek: ajan paneli", engine: "claude", project_id: null, pinned: false, archived: false, updated_at: "2026-01-01T12:00:00Z" }],
    events: [
      event(1, "session_started"),
      event(2, "assistant_text", { text: "Örnek dosyaları inceliyorum." }),
      event(3, "tool_call", { tool: "Task", input: { description: "Örnek test taraması" } }),
    ],
  };
}

test("agent_detail_streams_and_stops", async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 800 });
  await installFakeBridge(page, {
    ...data(),
    stream: { delayMs: 2500, eventsPerSecond: 2, events: [
      event(4, "tool_result", { tool: "Task", output_excerpt: "Örnek alt ajan testleri geçti", is_error: false }),
      event(5, "command", { command: "npm test -- example", exit_code: 0, output_excerpt: "Örnek testler geçti" }),
    ] },
  });
  await page.goto("/chat/chat-agents");
  const panel = page.getByRole("complementary", { name: "Sohbet ayrıntıları", exact: true });
  const row = panel.getByRole("button", { name: /Claude oturumu/ });
  await row.click();
  const detail = panel.getByRole("region", { name: "Claude oturumu", exact: true });
  await expect(detail).toBeVisible();
  await expect(detail.getByRole("button", { name: "‹ Ajanlar" })).toBeFocused();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  const composer = page.getByRole("textbox", { name: "Mesaj", exact: true });
  await composer.fill("Örnek: sohbet taslağı kullanılabilir");
  await expect(composer).toBeFocused();
  await expect(composer).toHaveValue("Örnek: sohbet taslağı kullanılabilir");

  const events = detail.getByRole("list", { name: "Ajan olayları" });
  await expect(events.getByText(/Örnek alt ajan testleri geçti/)).toBeVisible();
  await expect(events.getByText("npm test -- example · çıkış: 0")).toBeVisible();
  const queueAction = detail.getByRole("button", { name: "Kuyruğa al", exact: true });
  await expect(queueAction).toBeDisabled();
  await detail.getByRole("textbox", { name: "Ajana mesaj yaz" }).fill("Örnek: sonraki turda sonuçları açıkla");
  await expect(queueAction).toBeEnabled();
  await queueAction.click();
  await expect(detail.getByText("tur bitince iletilecek", { exact: true })).toBeVisible();
  await detail.getByRole("button", { name: "Durdur", exact: true }).click();
  await expect(detail.getByRole("button", { name: "Durdurma istendi", exact: true })).toBeDisabled();
  // İptal tamamlanma olayı üretmez: mesaj gerçek turn_completed olayını bekler.
  await expect(detail.getByText("tur bitince iletilecek", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Gönder", exact: true })).toBeEnabled();
  await page.screenshot({ path: "e2e/__screenshots__/agent-detail-1280x800.png" });

  await detail.getByRole("button", { name: "Devral", exact: true }).click();
  await detail.getByRole("group", { name: "Devralacak motor" }).getByRole("button", { name: "Codex", exact: true }).click();
  await expect(detail.getByRole("group", { name: "Devralacak motor" })).toHaveCount(0);
  await detail.getByRole("button", { name: "‹ Ajanlar" }).focus();
  await page.keyboard.press("Escape");
  await expect(detail).toHaveCount(0);
  await expect(row).toBeFocused();
  await expect(page.getByRole("button", { name: "Motor: Codex" })).toBeVisible();
  await expect(composer).toHaveValue("Örnek: sohbet taslağı kullanılabilir");
});

test("agent_detail_back_with_reduced_motion", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await installFakeBridge(page, data());
  await page.goto("/chat/chat-agents");
  const panel = page.getByRole("complementary", { name: "Sohbet ayrıntıları", exact: true });
  const row = panel.getByRole("button", { name: /Örnek test taraması/ });
  await row.click();
  const detail = panel.getByRole("region", { name: "Örnek test taraması", exact: true });
  await expect(detail).toBeVisible();
  expect(await detail.evaluate((element) => getComputedStyle(element).transform)).toBe("none");
  await detail.getByRole("button", { name: "‹ Ajanlar" }).click();
  await expect(detail).toHaveCount(0);
  await expect(row).toBeFocused();
});

test("agent_list_empty_state", async ({ page }) => {
  await installFakeBridge(page, { ...data(), events: [] });
  await page.goto("/chat/chat-agents");
  const panel = page.getByRole("complementary", { name: "Sohbet ayrıntıları", exact: true });
  await expect(panel.getByText(/Henüz ajan oturumu yok/)).toBeVisible();
  await expect(panel.getByRole("button", { name: /Claude oturumu/ })).toHaveCount(0);
});
