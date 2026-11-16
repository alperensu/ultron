import { expect, test } from "@playwright/test";
import { installFakeBridge } from "./fakeData";

test("sidebar_navigation_and_context_menu", async ({ page }) => {
  await installFakeBridge(page);
  await page.goto("/");
  const sidebar = page.getByRole("complementary", { name: "Kenar çubuğu" });

  // Maket v2 sırası; arka ucu olmayan Bildirimler/Hafıza yok, üstte sekme çubuğu yok.
  for (const name of ["Yeni sohbet", "Ara", "Bugün", "Ayarlar"]) {
    await expect(sidebar.getByRole("button", { name, exact: true })).toBeVisible();
  }
  for (const heading of ["Sabitlenenler", "Projeler", "Projesiz sohbetler"]) {
    await expect(sidebar.getByText(heading, { exact: true })).toBeVisible();
  }
  await expect(page.getByText("Bildirimler")).toHaveCount(0);
  await expect(page.getByText("Hafıza")).toHaveCount(0);
  await expect(page.getByRole("tablist")).toHaveCount(0);
  await expect(sidebar.getByText("Servis hazır")).toBeVisible();

  // Proje açılır, sohbet açılınca rota değişir.
  await sidebar.getByRole("button", { name: /Ultron/ }).click();
  await sidebar.getByRole("button", { name: "Masaüstü iskeleti", exact: true }).click();
  await expect(page).toHaveURL(/\/chat\/c1$/);
  await expect(page.getByRole("heading", { name: "Masaüstü iskeleti" })).toBeVisible();

  // Sağ tık: yeniden adlandır.
  await sidebar.getByRole("button", { name: "Kota paneli taslağı", exact: true }).click({ button: "right" });
  await page.getByRole("menuitem", { name: "Yeniden adlandır" }).click();
  await page.getByLabel("Sohbet adı").fill("Kota paneli");
  await page.keyboard.press("Enter");
  await expect(sidebar.getByRole("button", { name: "Kota paneli", exact: true })).toBeVisible();

  // "…" menüsü: sabitle.
  const row = sidebar.locator(".chat-row", { hasText: "Kota paneli" });
  await row.hover();
  await row.getByRole("button", { name: /Sohbet menüsü/ }).click();
  await page.getByRole("menuitem", { name: "Sabitle", exact: true }).click();
  await expect(sidebar.locator(".chat-row", { hasText: "Kota paneli" }).locator(".chat-row__pin")).toBeVisible();

  // Projeye taşı: Projesiz "Haftalık plan" -> Mobil uygulama.
  await sidebar.getByRole("button", { name: "Haftalık plan", exact: true }).click({ button: "right" });
  await page.getByRole("menuitem", { name: "Projeye taşı" }).click();
  const dialog = page.getByRole("dialog", { name: "Projeye taşı" });
  await dialog.getByRole("button", { name: "Mobil uygulama" }).click();
  await expect(dialog).toHaveCount(0);
  await sidebar.getByRole("button", { name: /Mobil uygulama/ }).click();
  await expect(sidebar.locator(".chat-row.is-nested", { hasText: "Haftalık plan" })).toHaveCount(0); // sabitli kalır

  // Arşivle: listeden kalkar.
  await sidebar.getByRole("button", { name: "Eski deneme", exact: true }).click({ button: "right" });
  await page.getByRole("menuitem", { name: "Arşivle" }).click();
  await expect(sidebar.getByRole("button", { name: "Eski deneme", exact: true })).toHaveCount(0);

  // Ara: historySearch sonuçları.
  await sidebar.getByRole("button", { name: "Ara", exact: true }).click();
  await page.getByLabel("Geçmişte ara").fill("halka");
  await expect(sidebar.getByText("kota paneli için halka ve çubuk tasarımı")).toBeVisible();
  await sidebar.locator(".sidebar-hit").click();
  await expect(page).toHaveURL(/\/chat\/c2$/);
});

test("composer sends to a new chat and opens the chat screen", async ({ page }) => {
  await installFakeBridge(page);
  await page.goto("/");
  await page.getByLabel("Mesaj").fill("yeni bir fikir");
  await expect(page.getByRole("button", { name: /Proje: → Ultron \(otomatik\)/ })).toBeVisible();
  await page.getByLabel("Mesaj").press("Enter");
  await expect(page).toHaveURL(/\/chat\/sohbet-\d+$/);
  await expect(page.getByText("yeni bir fikir").first()).toBeVisible();
});
