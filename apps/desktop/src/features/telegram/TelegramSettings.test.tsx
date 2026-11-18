import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import {
  createFakeBridge, type FakeBridgeOptions, type TelegramSettings as Settings, type TelegramStatusSnapshot,
  type TelegramTestReport,
} from "../../lib/bridge";
import { createAppStore, StoreProvider } from "../../state/store";
import { TelegramSettings } from "./TelegramSettings";

const defaults: Settings = {
  notify: "always", auto_continue_enabled: true, auto_continue_wait_minutes: 45, fallback_order: [],
};
const connected: TelegramStatusSnapshot = {
  state: "connected", error: null, allowed_user_count: 3, settings: defaults,
};

function setup(options?: FakeBridgeOptions) {
  const bridge = createFakeBridge({ telegramStatus: connected, ...options });
  const ui = render(<StoreProvider value={createAppStore(bridge)}><TelegramSettings /></StoreProvider>);
  return { ...ui, bridge, user: userEvent.setup() };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((resolvePromise) => { resolve = resolvePromise; });
  return { promise, resolve };
}

const save = () => screen.getByRole("button", { name: "Telegram ayarlarını kaydet" });
const waitField = () => screen.getByRole("spinbutton", { name: "Bekleme süresi (dakika)" });

it.each([
  ["connected", null, 3, "Bağlı"],
  ["missing_token", null, 3, "Token yok"],
  ["missing_allowed_users", null, 0, "İzinli kullanıcı yok"],
  ["error", "network", 3, "Hata"],
] as const)("renders the daemon status %s and a count only", async (state, error, count, label) => {
  const { container } = setup({
    telegramStatus: { ...connected, state, error, allowed_user_count: count },
  });
  expect(await screen.findByText(label)).toBeVisible();
  expect(screen.getByText("İzinli kullanıcı sayısı").parentElement).toHaveTextContent(String(count));
  if (error) expect(screen.getByText("Telegram ağına erişilemedi. (network)")).toBeVisible();
  expect(screen.getByText("ultron telegram set-token")).toBeVisible();
  expect(container.querySelectorAll("input")).toHaveLength(2);
  expect(container.querySelector("input[type='password'], textarea")).toBeNull();
  if (state === "missing_token" || state === "missing_allowed_users") {
    expect(screen.getByRole("button", { name: "Test mesajı gönder" })).toBeDisabled();
  }
});

it("validates the wait and unique fallback order, and round-trips settings", async () => {
  const { bridge, user } = setup();
  await screen.findByRole("button", { name: "Telegram ayarlarını kaydet" });
  expect(waitField()).toHaveValue(45);
  const enabled = screen.getByRole("checkbox", { name: "Kota dolduğunda otomatik devam et" });
  expect(enabled).toBeChecked();
  await user.click(enabled);
  await user.selectOptions(screen.getByLabelText("Bildirim modu"), "away");
  await user.selectOptions(screen.getByLabelText("İlk tercih"), "agy");
  await user.selectOptions(screen.getByLabelText("İkinci tercih"), "agy");
  expect(save()).toBeDisabled();
  expect(screen.getByText("Her motoru bir kez seç.")).toBeVisible();
  await user.selectOptions(screen.getByLabelText("İkinci tercih"), "codex");
  for (const invalid of ["0", "1441", "1.5"]) {
    await user.clear(waitField());
    await user.type(waitField(), invalid);
    expect(save()).toBeDisabled();
  }
  await user.clear(waitField());
  expect(save()).toBeDisabled();
  await user.type(waitField(), "1440");
  await user.click(save());
  const settings = {
    notify: "away", auto_continue_enabled: false,
    auto_continue_wait_minutes: 1440, fallback_order: ["agy", "codex"],
  };
  expect(await screen.findByText("Telegram ayarları kaydedildi.")).toBeVisible();
  expect(bridge.calls).toContainEqual({ cmd: "set_telegram_settings", args: [settings] });
  expect(await bridge.getTelegramSettings()).toEqual(settings);
  await user.click(screen.getByRole("button", { name: "Durumu yenile" }));
  expect(await screen.findByRole("spinbutton")).toHaveValue(1440);
  expect(screen.getByLabelText("İlk tercih")).toHaveValue("agy");
  expect(screen.getByLabelText("İkinci tercih")).toHaveValue("codex");
  expect(screen.getByLabelText("Üçüncü tercih")).toHaveValue("");
  expect(screen.getByRole("checkbox", { name: "Kota dolduğunda otomatik devam et" })).not.toBeChecked();
});

it("retains an unsaved draft after rejection and omits raw errors", async () => {
  const { user, bridge } = setup();
  vi.spyOn(bridge, "setTelegramSettings").mockRejectedValue(new Error("synthetic-raw-error"));
  const wait = await screen.findByRole("spinbutton");
  await user.clear(wait);
  await user.type(wait, "1");
  await user.click(save());
  expect(await screen.findByRole("alert")).toHaveTextContent("kaydedildiği doğrulanamadı");
  expect(screen.queryByText("Telegram ayarları kaydedildi.")).not.toBeInTheDocument();
  expect(screen.queryByText(/synthetic-raw-error/)).not.toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "Durumu yenile" }));
  expect(await screen.findByRole("spinbutton")).toHaveValue(1);
  expect(screen.getByText("Kaydedilmemiş değişiklikler var.")).toBeVisible();
  expect(await bridge.getTelegramSettings()).toEqual(defaults);
});

it("ignores a late save from a previous connection after loading the new form", async () => {
  const { bridge, user, rerender } = setup();
  const pending = deferred<Settings>();
  vi.spyOn(bridge, "setTelegramSettings").mockReturnValue(pending.promise);
  await screen.findByRole("spinbutton");
  await user.click(save());
  expect(screen.getByRole("button", { name: "Kaydediliyor…" })).toBeDisabled();

  const newSettings = { ...defaults, auto_continue_wait_minutes: 80 };
  const nextBridge = createFakeBridge({ telegramStatus: { ...connected, settings: newSettings } });
  rerender(<StoreProvider value={createAppStore(nextBridge)}><TelegramSettings /></StoreProvider>);
  expect(await screen.findByRole("spinbutton")).toHaveValue(80);
  await act(async () => { pending.resolve({ ...defaults, auto_continue_wait_minutes: 90 }); });
  expect(waitField()).toHaveValue(80);
  expect(screen.queryByText("Telegram ayarları kaydedildi.")).not.toBeInTheDocument();
});

it("reloads once more when a save finishes after the connection changed", async () => {
  const { bridge, user, rerender } = setup();
  const pending = deferred<Settings>();
  vi.spyOn(bridge, "setTelegramSettings").mockReturnValue(pending.promise);
  await screen.findByRole("spinbutton");
  await user.click(save());
  const nextBridge = createFakeBridge({ telegramStatus: connected });
  const status = vi.spyOn(nextBridge, "telegramStatus");
  rerender(<StoreProvider value={createAppStore(nextBridge)}><TelegramSettings /></StoreProvider>);
  await screen.findByRole("spinbutton");
  expect(status).toHaveBeenCalledTimes(1);
  await act(async () => { pending.resolve({ ...defaults, auto_continue_wait_minutes: 90 }); });
  await vi.waitFor(() => expect(status).toHaveBeenCalledTimes(2));
  expect(await screen.findByRole("spinbutton")).toHaveValue(45);
});

it("shows an honest missing-report state when the test times out and re-enables the controls", async () => {
  const { bridge, user } = setup();
  vi.spyOn(bridge, "telegramTest").mockRejectedValue(new Error("synthetic-timeout"));
  await user.click(await screen.findByRole("button", { name: "Test mesajı gönder" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("teslim raporu alınamadı");
  expect(screen.queryByText(/synthetic-timeout/)).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Test mesajı gönder" })).toBeEnabled();
  expect(save()).toBeEnabled();
});

it("ignores a late status response after a newer connection has loaded", async () => {
  const old = createFakeBridge();
  const pending = deferred<TelegramStatusSnapshot>();
  vi.spyOn(old, "telegramStatus").mockReturnValue(pending.promise);
  const { rerender } = render(<StoreProvider value={createAppStore(old)}><TelegramSettings /></StoreProvider>);
  const next = createFakeBridge({
    telegramStatus: { ...connected, state: "missing_allowed_users", allowed_user_count: 0 },
  });
  rerender(<StoreProvider value={createAppStore(next)}><TelegramSettings /></StoreProvider>);
  expect(await screen.findByText("İzinli kullanıcı yok")).toBeVisible();
  await act(async () => { pending.resolve(connected); });
  expect(screen.getByText("İzinli kullanıcı yok")).toBeVisible();
  expect(screen.queryByText("Bağlı")).not.toBeInTheDocument();
});

it("only sends a single test on a double click and shows every partial delivery count", async () => {
  const { bridge, user } = setup();
  const pending = deferred<TelegramTestReport>();
  const test = vi.spyOn(bridge, "telegramTest").mockReturnValue(pending.promise);
  await user.dblClick(await screen.findByRole("button", { name: "Test mesajı gönder" }));
  expect(test).toHaveBeenCalledTimes(1);
  expect(screen.getByRole("button", { name: "Test mesajı gönderiliyor…" })).toBeDisabled();
  expect(screen.queryByRole("status", { name: "Telegram test sonucu" })).not.toBeInTheDocument();
  await act(async () => { pending.resolve({ attempted: 3, delivered: 1, failed: 1, uncertain: 1 }); });
  const report = within(await screen.findByRole("status", { name: "Telegram test sonucu" }));
  expect(report.getByText("Test mesajı kısmen teslim edildi.")).toBeVisible();
  for (const [label, count] of [["Denenen", 3], ["Teslim edilen", 1], ["Başarısız", 1], ["Belirsiz", 1]]) {
    expect(report.getByText(String(label)).nextElementSibling).toHaveTextContent(String(count));
  }
});

it.each([
  [{ attempted: 2, delivered: 2, failed: 0, uncertain: 0 }, "Test mesajı tüm hedeflere teslim edildi."],
  [{ attempted: 2, delivered: 0, failed: 2, uncertain: 0 }, "Test mesajı hiçbir kullanıcıya teslim edilmedi."],
  [{ attempted: 2, delivered: 0, failed: 1, uncertain: 1 }, "Test mesajının teslimi doğrulanamadı."],
  [{ attempted: 0, delivered: 0, failed: 0, uncertain: 0 }, "Test mesajı için gönderim yapılmadı."],
] as const)("distinguishes test outcomes for %o", async (telegramTestReport, message) => {
  const { user } = setup({ telegramTestReport });
  await user.click(await screen.findByRole("button", { name: "Test mesajı gönder" }));
  expect(await screen.findByRole("status", { name: "Telegram test sonucu" })).toHaveTextContent(message);
});
