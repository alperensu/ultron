import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import { createFakeBridge, type ImportReport } from "../../lib/bridge";
import { Settings } from "../../screens/Settings";
import { createAppStore, StoreProvider } from "../../state/store";

function setup() {
  const bridge = createFakeBridge({
    catalogImportReport: { imported: 3, updated: 2 },
    historyImportReport: { files: 4, chats: 3, messages: 18, skipped: 2, masked: 5 },
  });
  render(<StoreProvider value={createAppStore(bridge)}><Settings /></StoreProvider>);
  return bridge;
}

it("import_report_rendered", async () => {
  const bridge = setup();
  const user = userEvent.setup();
  await user.type(screen.getByRole("textbox", { name: "Katalog dosyasının yolu" }), "/ornek/katalog.md");
  await user.click(screen.getByRole("button", { name: "Proje kataloğunu içe aktar" }));
  const catalog = await screen.findByRole("status", { name: "Katalog içe aktarma sonucu" });
  expect(within(catalog).getByText("Eklenen proje").nextElementSibling).toHaveTextContent("3");
  expect(within(catalog).getByText("Güncellenen proje").nextElementSibling).toHaveTextContent("2");
  expect(bridge.calls).toContainEqual({ cmd: "import_catalog", args: ["/ornek/katalog.md"] });

  await user.click(screen.getByRole("button", { name: "Claude Code geçmişini içe aktar" }));
  const history = await screen.findByRole("status", { name: "Geçmiş içe aktarma sonucu" });
  for (const [label, count] of [["Dosya", 4], ["Sohbet", 3], ["Mesaj", 18], ["Atlanan", 2], ["Maskelenen", 5]] as const) {
    expect(within(history).getByText(label).nextElementSibling).toHaveTextContent(String(count));
  }
  expect(bridge.calls).toContainEqual({ cmd: "import_claude_history", args: [undefined] });
});

it("history_import_has_pending_and_error_states_without_duplicate_submission", async () => {
  const bridge = setup();
  let reject!: (reason: unknown) => void;
  const pending = new Promise<ImportReport>((_resolve, rejectPromise) => { reject = rejectPromise; });
  const command = vi.spyOn(bridge, "importClaudeHistory").mockReturnValue(pending);
  const user = userEvent.setup();
  await user.dblClick(screen.getByRole("button", { name: "Claude Code geçmişini içe aktar" }));
  expect(command).toHaveBeenCalledTimes(1);
  expect(screen.getByRole("button", { name: "Geçmiş içe aktarılıyor…" })).toBeDisabled();
  expect(screen.getByText("Dosyalar işleniyor; işlem bitince sonuç burada görünecek.")).toBeVisible();
  await act(async () => { reject({ kind: "daemon", message: "Geçmiş okunamadı" }); });
  expect(await screen.findByRole("alert")).toHaveTextContent("Geçmiş okunamadı");
  expect(screen.queryByRole("status", { name: "Geçmiş içe aktarma sonucu" })).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Claude Code geçmişini içe aktar" })).toBeEnabled();
});

it("unbacked_settings_are_read_only_and_catalog_requires_a_path", () => {
  setup();
  expect(screen.queryByRole("switch")).not.toBeInTheDocument();
  expect(screen.getByText("Claude kotasını canlı oku")).toBeVisible();
  expect(screen.getAllByText(/config.toml içinden değiştirilir/).length).toBeGreaterThan(0);
  expect(screen.getByRole("button", { name: "Proje kataloğunu içe aktar" })).toBeDisabled();
});
