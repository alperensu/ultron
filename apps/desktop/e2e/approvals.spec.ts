import { expect, test } from "@playwright/test";
import { installFakeBridge } from "./fakeData";

test("today_shows_pending_approvals_and_approves_with_keyboard", async ({ page }) => {
  await installFakeBridge(page, { approvals: [{
    id: "example-approval", chat_id: null, engine: "claude", category: "Delete", reason: "Örnek: dosyayı silmek için onay gerekiyor.",
    action_json: JSON.stringify({ Shell: { command: "rm example.txt; token=[MASKED]", cwd: "/example" } }),
    status: "pending", created_at: new Date().toISOString(), resolved_at: null, resolved_by: null,
  }] });
  await page.goto("/");
  await expect(page.getByText("Seni bekleyen", { exact: true })).toBeVisible();
  const card = page.getByRole("article", { name: "Dosya silme" });
  await expect(card).toBeVisible();
  await expect(card.getByLabel("Maskeli işlem özeti")).toContainText("token=[MASKED]");
  await card.getByRole("button", { name: "Onayla", exact: true }).focus();
  await page.keyboard.press("Enter");
  await expect(card).toHaveCount(0);
  await expect(page.getByText("Onaylandı.", { exact: true })).toBeVisible();
  await expect(page.getByText("Bekleyen onay yok", { exact: true })).toBeVisible();
});
