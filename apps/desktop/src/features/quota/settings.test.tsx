import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import { createFakeBridge, type ProjectImportReport } from "../../lib/bridge";
import { Settings } from "../../screens/Settings";
import { createAppStore, StoreProvider } from "../../state/store";

function setup() {
  const bridge = createFakeBridge({
    catalogImportReport: { imported: 3, updated: 2 },
    projectImportReport: {
      dry_run: false,
      rows: [
        { path: "/ornek/a", name: "a", claude_sessions: 2, codex_sessions: 1, worker_sessions: 0, project_created: true, note_updated: true, note_status: "güncellendi" },
        { path: "/ornek/b", name: "b", claude_sessions: 1, codex_sessions: 0, worker_sessions: 3, project_created: false, note_updated: false, note_status: "güncel" },
      ],
      created: 1,
      notes_updated: 1,
      summarizer: "null",
      unmatched_sessions: 0,
    },
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

  await user.click(screen.getByRole("button", { name: "Projeleri içe aktar" }));
  const history = await screen.findByRole("status", { name: "Proje içe aktarma sonucu" });
  for (const [label, count] of [["Proje", 2], ["Yeni proje", 1], ["Güncellenen not", 1]] as const) {
    expect(within(history).getByText(label).nextElementSibling).toHaveTextContent(String(count));
  }
  expect(bridge.calls).toContainEqual({ cmd: "import_projects", args: [] });
});

it("history_import_has_pending_and_error_states_without_duplicate_submission", async () => {
  const bridge = setup();
  let reject!: (reason: unknown) => void;
  const pending = new Promise<ProjectImportReport>((_resolve, rejectPromise) => { reject = rejectPromise; });
  const command = vi.spyOn(bridge, "importProjects").mockReturnValue(pending);
  const user = userEvent.setup();
  await user.dblClick(screen.getByRole("button", { name: "Projeleri içe aktar" }));
  expect(command).toHaveBeenCalledTimes(1);
  expect(screen.getByRole("button", { name: "Projeler içe aktarılıyor…" })).toBeDisabled();
  expect(screen.getByText("Oturumlar işleniyor; işlem bitince sonuç burada görünecek.")).toBeVisible();
  await act(async () => { reject({ kind: "daemon", message: "Geçmiş okunamadı" }); });
  expect(await screen.findByRole("alert")).toHaveTextContent("Geçmiş okunamadı");
  expect(screen.queryByRole("status", { name: "Proje içe aktarma sonucu" })).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Projeleri içe aktar" })).toBeEnabled();
});

it("unbacked_settings_are_read_only_and_catalog_requires_a_path", () => {
  setup();
  expect(screen.queryByRole("switch")).not.toBeInTheDocument();
  expect(screen.getByText("Claude kotasını canlı oku")).toBeVisible();
  expect(screen.getAllByText(/config.toml içinden değiştirilir/).length).toBeGreaterThan(0);
  expect(screen.getByRole("button", { name: "Proje kataloğunu içe aktar" })).toBeDisabled();
});
