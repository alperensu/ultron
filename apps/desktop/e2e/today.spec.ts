import { expect, test } from "@playwright/test";
import { installFakeBridge } from "./fakeData";

test("today_shows_recent_turns", async ({ page }) => {
  await installFakeBridge(page);
  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Bugün", level: 1 })).toBeVisible();
  await expect(page.getByText("Son 24 saatte 2 tur bitti.")).toBeVisible();

  // "Sen yokken": yalnız son 24 saatte biten turlar; 3 gün önceki yok.
  const away = page.locator("section", { has: page.getByRole("heading", { name: "Sen yokken" }) });
  await expect(away.getByRole("button")).toHaveCount(2);
  await expect(away.getByRole("button", { name: /Masaüstü iskeleti/ })).toContainText("Claude · 14 dk 0 sn");
  await expect(away.getByRole("button", { name: /Kota paneli taslağı/ })).toContainText("Codex");
  await expect(away.getByText("Eski deneme")).toHaveCount(0);

  // Kota şeridi: bilinmeyen havuz uydurma yüzde göstermez.
  await expect(page.getByText("Claude").first()).toBeVisible();
  await expect(page.getByText("5 saat · %42")).toBeVisible();
  await expect(page.getByText("Haftalık · %87 · tahmini")).toBeVisible();
  await expect(page.getByText("Günlük · bilinmiyor")).toBeVisible();

  // Üst çubukta zil yok; kota halkası var.
  await expect(page.getByRole("button", { name: /Bildirim/ })).toHaveCount(0);
  await expect(page.locator(".navbar-quota")).toBeVisible();
  await expect(page.getByText("Seni bekleyen")).toHaveCount(0);
  await expect(page.getByText("Önerim")).toHaveCount(0);

  await page.waitForTimeout(700); // giriş animasyonları
  await page.screenshot({ path: "e2e/__screenshots__/today-1280x800.png" });
  await page.setViewportSize({ width: 1920, height: 1080 });
  await page.waitForTimeout(300);
  await page.screenshot({ path: "e2e/__screenshots__/today-1920x1080.png" });
});

test("today_empty_states_are_honest", async ({ page }) => {
  await installFakeBridge(page, { projects: [], chats: [], events: [] });
  await page.goto("/");
  await expect(page.getByText("Son 24 saatte biten tur yok.")).toBeVisible();
  await expect(page.getByText("Henüz biten tur yok")).toBeVisible();
  await expect(page.getByText("Kota bilgisi yok")).toBeVisible();
  await expect(page.getByText("Henüz proje yok")).toBeVisible();
});
