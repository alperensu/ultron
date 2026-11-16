import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import { createFakeBridge, type QuotaReport } from "../../lib/bridge";
import { createAppStore, StoreProvider } from "../../state/store";
import { NavQuota } from "./NavQuota";
import { QuotaProvider } from "./QuotaProvider";
import { resetCountdown } from "./QuotaSheet";

// Yalnız test: sentetik kota pencereleri ve hata notu.
const report: QuotaReport = {
  fetched_at: "2026-01-01T00:00:00Z",
  pools: [
    {
      pool: "claude", mode: "Tam", reset_rights: 1,
      windows: [{ label: "5 saatlik", used_pct: 45, resets_at: null, window_secs: 18000, source: "Live" }],
    },
    {
      pool: "codex", mode: "Eko", reset_rights: 2, note: "Canlı ölçüm alınamadı; tahmini değer.",
      windows: [{ label: "Haftalık", used_pct: 95, resets_at: null, window_secs: 604800, source: "Estimated" }],
    },
    {
      pool: "agy", mode: "Dengeli", reset_rights: null,
      windows: [{ label: "Günlük", used_pct: null, resets_at: null, window_secs: 86400, source: "Unknown" }],
    },
  ],
};

const stops: Array<() => void> = [];
afterEach(() => {
  stops.splice(0).forEach((stop) => stop());
  vi.useRealTimers();
});

async function setup(quota = report) {
  const bridge = createFakeBridge({ quota });
  const store = createAppStore(bridge);
  const view = render(
    <StoreProvider value={store}>
      <QuotaProvider><NavQuota /></QuotaProvider>
    </StoreProvider>,
  );
  await act(async () => { stops.push(store.start()); });
  return { bridge, ...view };
}

it("quota_sheet_shows_source_labels", async () => {
  const { bridge } = await setup();
  const user = userEvent.setup();
  expect(screen.getByRole("img", { name: "Codex Haftalık: %95" })).toHaveAttribute("data-tone", "crit");
  const initialCalls = bridge.calls.filter((call) => call.cmd === "get_quota").length;
  await user.click(screen.getByRole("button", { name: "Kota panelini aç" }));

  const sheet = await screen.findByRole("dialog", { name: "Kota" });
  expect(sheet.parentElement).toBe(document.body);
  for (const source of ["Canlı", "Tahmini", "Bilinmiyor"]) {
    expect(within(sheet).getAllByText(source).length).toBeGreaterThan(0);
  }
  for (const mode of ["Tam", "Dengeli", "Eko"]) expect(within(sheet).getByText(mode)).toBeVisible();
  expect(within(sheet).getByText("Canlı ölçüm alınamadı; tahmini değer.")).toBeVisible();
  expect(within(sheet).getByRole("progressbar", { name: "Antigravity Günlük" })).not.toHaveAttribute("aria-valuenow");
  expect(bridge.calls.filter((call) => call.cmd === "get_quota")).toHaveLength(initialCalls + 1);
  expect(within(sheet).queryByRole("spinbutton", { name: /Antigravity/ })).not.toBeInTheDocument();
});

it("saves_reset_rights_and_refreshes_quota", async () => {
  const { bridge } = await setup();
  const user = userEvent.setup();
  await user.click(screen.getByRole("button", { name: "Kota panelini aç" }));
  const input = await screen.findByRole("spinbutton", { name: "Claude sıfırlama hakkı" });
  await user.clear(input);
  await user.type(input, "4");
  const form = input.closest("form")!;
  await user.click(within(form).getByRole("button", { name: "Kaydet" }));
  expect(await within(form).findByText("Sıfırlama hakkı kaydedildi.")).toBeVisible();
  expect(bridge.calls).toContainEqual({ cmd: "set_reset_rights", args: ["claude", 4] });
  expect((await bridge.getQuota()).pools[0]!.reset_rights).toBe(4);
  expect(report.pools[0]!.reset_rights).toBe(1);
});

it("reset_rights_refresh_waits_for_an_older_quota_request", async () => {
  const { bridge } = await setup();
  let resolve!: (value: QuotaReport) => void;
  const olderRequest = new Promise<QuotaReport>((resolvePromise) => { resolve = resolvePromise; });
  vi.spyOn(bridge, "getQuota").mockImplementationOnce(() => olderRequest);
  const user = userEvent.setup();
  await user.click(screen.getByRole("button", { name: "Kota panelini aç" }));
  const input = screen.getByRole("spinbutton", { name: "Claude sıfırlama hakkı" });
  await user.clear(input);
  await user.type(input, "4");
  await user.click(within(input.closest("form")!).getByRole("button", { name: "Kaydet" }));
  await waitFor(() => expect(bridge.calls).toContainEqual({ cmd: "set_reset_rights", args: ["claude", 4] }));
  await act(async () => { resolve(structuredClone(report)); });
  await waitFor(() => expect(input).toHaveValue(4));
  expect((await bridge.getQuota()).pools[0]!.reset_rights).toBe(4);
});

it("refreshes_every_60_seconds_and_clears_timer_on_unmount", async () => {
  const { bridge, unmount } = await setup();
  vi.useFakeTimers();
  // Zamanlayıcı zaten kurulmuş olduğundan yeni bir render üzerinden sahte saat kurulur.
  unmount();
  const store = createAppStore(bridge);
  const view = render(
    <StoreProvider value={store}>
      <QuotaProvider><NavQuota /></QuotaProvider>
    </StoreProvider>,
  );
  const before = bridge.calls.filter((call) => call.cmd === "get_quota").length;
  await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
  expect(bridge.calls.filter((call) => call.cmd === "get_quota")).toHaveLength(before + 1);
  view.unmount();
  await act(async () => { await vi.advanceTimersByTimeAsync(60_000); });
  expect(bridge.calls.filter((call) => call.cmd === "get_quota")).toHaveLength(before + 1);
});

it("quota_failure_keeps_panel_accessible_without_a_fake_percent", async () => {
  const { bridge } = await setup();
  vi.spyOn(bridge, "getQuota").mockRejectedValue({ kind: "daemon", message: "Ölçüm okunamadı" });
  const user = userEvent.setup();
  await user.click(screen.getByRole("button", { name: "Kota panelini aç" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Ölçüm okunamadı");
  await waitFor(() => expect(screen.queryByRole("img")).not.toBeInTheDocument());
  expect(screen.getByRole("button", { name: "Kota panelini aç" })).toBeVisible();
});

it("formats_reset_countdown_without_inventing_missing_times", () => {
  const now = Date.parse("2026-01-01T00:00:00Z");
  expect(resetCountdown("2026-01-01T02:10:00Z", now)).toBe("sıfırlanma 2 sa 10 dk");
  expect(resetCountdown(null, now)).toBe("Sıfırlanma zamanı bilinmiyor");
  expect(resetCountdown("invalid", now)).toBe("Sıfırlanma zamanı bilinmiyor");
});
