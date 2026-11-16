import { expect, test } from "@playwright/test";
import { installFakeBridge, sampleBridge } from "./fakeData";

test("settings_live_quota_is_read_only_note", async ({ page }) => {
  await installFakeBridge(page, {
    ...sampleBridge(),
    defaultEngine: "codex",
    catalogImportReport: { imported: 2, updated: 1 },
    historyImportReport: { files: 3, chats: 2, messages: 12, skipped: 1, masked: 4 },
  });
  await page.goto("/ayarlar");
  await expect(page.getByRole("heading", { name: "Ayarlar", level: 1 })).toBeVisible();
  const row = page.locator(".ui-row", { hasText: "Claude kotasını canlı oku" });
  await expect(row).toContainText("config.toml içinden değiştirilir");
  await expect(row).toContainText("mevcut değerini bildirmiyor");
  await expect(row.getByRole("switch")).toHaveCount(0);
  await expect(row.getByRole("button")).toHaveCount(0);
  await expect(page.locator(".ui-row", { hasText: "Varsayılan motor" })).toContainText("Codex");

  await page.getByRole("textbox", { name: "Katalog dosyasının yolu" }).fill("/ornek/katalog.md");
  await page.getByRole("button", { name: "Proje kataloğunu içe aktar" }).click();
  await expect(page.getByRole("status", { name: "Katalog içe aktarma sonucu" })).toContainText("Eklenen proje2");
  await page.getByRole("button", { name: "Claude Code geçmişini içe aktar" }).click();
  const report = page.getByRole("status", { name: "Geçmiş içe aktarma sonucu" });
  await expect(report).toContainText("Dosya3");
  await expect(report).toContainText("Sohbet2");
  await expect(report).toContainText("Mesaj12");
  await expect(report).toContainText("Atlanan1");
  await expect(report).toContainText("Maskelenen4");
});

test("quota_sheet_opens_from_ring", async ({ page }) => {
  const data = sampleBridge();
  const codex = data.quota!.pools.find((pool) => pool.pool === "codex")!;
  codex.windows[0]!.used_pct = 95;
  codex.windows[0]!.resets_at = new Date(Date.now() + 130 * 60_000).toISOString();
  codex.reset_rights = 2;
  codex.note = "Tahmini kullanım; canlı ölçüm alınamadı.";
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.setViewportSize({ width: 1280, height: 800 });
  await installFakeBridge(page, data);
  await page.goto("/");
  const trigger = page.getByRole("button", { name: "Kota panelini aç" });
  await expect(trigger.getByRole("img")).toHaveAttribute("data-tone", "crit");
  await trigger.click();
  const sheet = page.getByRole("dialog", { name: "Kota", exact: true });
  await expect(sheet).toBeVisible();
  // Portal, backdrop-filter içeren başlık çubuğunun containing block'undan çıkar.
  await expect(sheet.locator("..")).toHaveJSProperty("tagName", "BODY");
  const box = await sheet.boundingBox();
  expect(box!.y).toBe(0);
  expect(box!.height).toBe(800);
  expect(box!.x + box!.width).toBe(1280);
  await expect(sheet.getByText("Canlı", { exact: true })).toBeVisible();
  await expect(sheet.getByText("Tahmini", { exact: true })).toBeVisible();
  await expect(sheet.getByText("Bilinmiyor", { exact: true }).first()).toBeVisible();
  await expect(sheet.getByText(/^sıfırlanma 2 sa/)).toBeVisible();
  await expect(sheet.getByText(codex.note)).toBeVisible();
  await expect(sheet.getByRole("button", { name: "Yenile" })).toBeEnabled();
  await page.screenshot({ path: "e2e/__screenshots__/quota-1280x800.png" });

  const input = sheet.getByRole("spinbutton", { name: "Codex sıfırlama hakkı" });
  await input.fill("3");
  await input.locator("..").getByRole("button", { name: "Kaydet" }).click();
  await expect(sheet.getByText("Sıfırlama hakkı kaydedildi.")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(sheet).toHaveCount(0);
  await expect(trigger).toBeFocused();
});
