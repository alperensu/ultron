import { expect, test } from "@playwright/test";
import { installFakeBridge, sampleBridge } from "./fakeData";

for (const width of [1280, 390]) {
  test(`Telegram settings and delivery counts at ${width}px`, async ({ page }, testInfo) => {
    await page.setViewportSize({ width, height: 800 });
    await page.emulateMedia({ reducedMotion: "reduce" });
    await installFakeBridge(page, {
      ...sampleBridge(),
      telegramStatus: {
        state: "connected", allowed_user_count: 4, error: null,
        settings: {
          notify: "always", auto_continue_enabled: true, auto_continue_wait_minutes: 45,
          fallback_order: ["codex"],
        },
      },
      telegramTestReport: { attempted: 4, delivered: 2, failed: 1, uncertain: 1 },
    });
    await page.goto("/ayarlar");
    const section = page.locator("section").filter({ has: page.getByRole("heading", { name: "Telegram", exact: true }) });
    await expect(section.getByText("Bağlı", { exact: true })).toBeVisible();
    await expect(section.locator(".ui-row", { hasText: "İzinli kullanıcı sayısı" })).toContainText("4");
    await expect(section.getByText("ultron telegram set-token", { exact: true })).toBeVisible();
    await expect(section.locator('input[type="password"], textarea')).toHaveCount(0);

    const mode = section.getByRole("combobox", { name: "Bildirim modu" });
    await mode.focus();
    await expect(mode).toBeFocused();
    await mode.press("Tab");
    const enabled = section.getByRole("checkbox", { name: "Kota dolduğunda otomatik devam et" });
    await expect(enabled).toBeFocused();
    await enabled.press("Space");
    await expect(enabled).not.toBeChecked();
    await mode.selectOption("away");
    const minutes = section.getByRole("spinbutton", { name: "Bekleme süresi (dakika)" });
    await expect(minutes).toHaveValue("45");
    await minutes.fill("90");
    const secondChoice = section.getByRole("combobox", { name: "İkinci tercih" });
    await secondChoice.selectOption("agy");
    const save = section.getByRole("button", { name: "Telegram ayarlarını kaydet" });
    await save.focus();
    await save.press("Enter");
    await expect(section.getByText("Telegram ayarları kaydedildi.")).toBeVisible();
    await section.getByRole("button", { name: "Durumu yenile" }).click();
    await expect(minutes).toHaveValue("90");
    await expect(mode).toHaveValue("away");
    await expect(secondChoice).toHaveValue("agy");

    const send = section.getByRole("button", { name: "Test mesajı gönder" });
    await send.focus();
    await send.press("Enter");
    const report = section.getByRole("status", { name: "Telegram test sonucu" });
    await expect(report).toContainText("Test mesajı kısmen teslim edildi.");
    await expect(report).toContainText("Denenen4");
    await expect(report).toContainText("Teslim edilen2");
    await expect(report).toContainText("Başarısız1");
    await expect(report).toContainText("Belirsiz1");
    expect((await section.boundingBox())!.width).toBeGreaterThan(230);
    expect(await section.evaluate((element) => element.scrollWidth <= element.clientWidth)).toBe(true);
    await page.screenshot({ path: testInfo.outputPath(`telegram-settings-${width}.png`) });
  });
}
