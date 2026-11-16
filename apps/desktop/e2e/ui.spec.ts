import { expect, test } from "@playwright/test";

test("/__ui kitchen sink renders and is screenshotted", async ({ page }) => {
  await page.goto("/__ui");
  await expect(page.getByTestId("kitchen-sink")).toBeVisible();
  await expect(page.getByRole("heading", { name: "Bileşenler" })).toBeVisible();
  await page.waitForTimeout(800); // giriş animasyonlarının bitmesi
  await page.screenshot({ path: "e2e/__screenshots__/ui-kitchen-sink.png" });

  await page.getByRole("button", { name: "Alt sayfa" }).click();
  await expect(page.getByRole("dialog", { name: "Alt sayfa" })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(page.getByRole("dialog")).toHaveCount(0);
});
