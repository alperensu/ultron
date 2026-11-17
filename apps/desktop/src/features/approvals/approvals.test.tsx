import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import { createFakeBridge, type Approval, type PolicySettings as PolicyData } from "../../lib/bridge";
import { createAppStore, StoreProvider } from "../../state/store";
import { ApprovalInbox } from "./ApprovalInbox";
import { ApprovalCard } from "./ApprovalCard";
import { ApprovalProvider } from "./ApprovalProvider";
import { PolicySettings } from "./PolicySettings";
import { TrashSettings } from "./TrashSettings";
import { createChatTimeline } from "../chat/model";

export const pendingApproval = (id = "example-approval"): Approval => ({
  id, chat_id: null, engine: "claude", category: "Delete", reason: "Örnek: dosya silme onayı gerekiyor.",
  action_json: JSON.stringify({ Shell: { command: "rm example.txt; token=[MASKED]", cwd: "/example" } }),
  status: "pending", created_at: new Date().toISOString(), resolved_at: null, resolved_by: null,
});

it("approval_card_resolves_and_disappears", async () => {
  const bridge = createFakeBridge({ approvals: [pendingApproval()] });
  const user = userEvent.setup();
  render(<StoreProvider value={createAppStore(bridge)}><ApprovalProvider><ApprovalInbox /></ApprovalProvider></StoreProvider>);
  const card = await screen.findByRole("article", { name: "Dosya silme" });
  expect(within(card).getByLabelText("Maskeli işlem özeti")).toHaveTextContent("token=[MASKED]");
  expect(within(card).getByLabelText("Kalan süre üst sınırı")).toHaveTextContent("En fazla");
  within(card).getByRole("button", { name: "Onayla" }).focus();
  await user.keyboard("{Enter}");
  await waitFor(() => expect(screen.queryByRole("article", { name: "Dosya silme" })).not.toBeInTheDocument());
  expect(screen.getByText("Onaylandı.")).toBeVisible();
  expect((await bridge.listApprovals({ status: "approved" })).approvals).toHaveLength(1);
  expect(bridge.calls.filter((call) => call.cmd === "resolve_approval")).toHaveLength(1);
});

it("reconciles_all_pages_and_chatless_resolutions", async () => {
  const approvals = Array.from({ length: 65 }, (_, i) => pendingApproval(`example-${String(i).padStart(3, "0")}`));
  const bridge = createFakeBridge({ approvals });
  render(<StoreProvider value={createAppStore(bridge)}><ApprovalProvider><ApprovalInbox /></ApprovalProvider></StoreProvider>);
  await screen.findByText("1 / 11");
  expect(bridge.calls.filter((call) => call.cmd === "list_approvals")).toHaveLength(3);
  await act(async () => {
    bridge.emit({ id: "resolved", chat_id: "", session_id: null, seq: 1, ts: new Date().toISOString(), kind: "approval_resolved", payload: { approval_id: approvals[0]!.id, status: "denied", by: "approver" } });
  });
  await screen.findByText("Reddedildi.");
  await waitFor(() => expect(bridge.calls.filter((call) => call.cmd === "list_approvals")).toHaveLength(5));
  // Fake köprü reddedileni listeden düşürür: 64 kalan onay = 2 sayfa (3 + 2 çağrı), hiçbiri sayfa sınırında kaybolmaz.
  expect(screen.queryByText(approvals[0]!.id)).not.toBeInTheDocument();
});

it("expired_card_cannot_resolve_and_errors_are_visible", async () => {
  const old = { ...pendingApproval(), created_at: new Date(Date.now() - 301_000).toISOString() };
  const resolve = vi.fn();
  const view = render(<ApprovalCard approval={old} onResolve={resolve} />);
  expect(screen.getByRole("button", { name: "Onayla" })).toBeDisabled();
  // Kart giriş animasyonu bitene kadar opaklık 0'dır.
  await waitFor(() => expect(screen.getByText(/Süre doldu/)).toBeVisible());
  view.unmount();
  const bridge = createFakeBridge({ failWith: { kind: "daemon", message: "Örnek: servis bağlantısı yok." } });
  render(<StoreProvider value={createAppStore(bridge)}><ApprovalProvider><ApprovalInbox /></ApprovalProvider></StoreProvider>);
  expect(await screen.findByRole("alert")).toHaveTextContent("servis bağlantısı yok");
  expect(resolve).not.toHaveBeenCalled();
});

it("live_request_reconciles_and_resolution_failure_remains_visible", async () => {
  const bridge = createFakeBridge();
  render(<StoreProvider value={createAppStore(bridge)}><ApprovalProvider><ApprovalInbox /></ApprovalProvider></StoreProvider>);
  await screen.findByText("Bekleyen onay yok");
  await act(async () => { bridge.addApproval(pendingApproval()); });
  await screen.findByRole("article", { name: "Dosya silme" });
  vi.spyOn(bridge, "resolveApproval").mockRejectedValueOnce({ kind: "daemon", message: "Örnek: karar kaydedilemedi." });
  await userEvent.setup().click(screen.getByRole("button", { name: "Reddet" }));
  await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("karar kaydedilemedi"));
  expect(screen.getByRole("article", { name: "Dosya silme" })).toBeInTheDocument();
});

it("policy_settings_roundtrip_preserves_readonly_rules", async () => {
  const policySettings: PolicyData = { config: { finance_paths: [], protected_paths: [], deploy_patterns: ["example-deploy"], allow_list: ["git status"] }, trash_retention_days: 30 };
  const bridge = createFakeBridge({ policySettings });
  const user = userEvent.setup();
  render(<StoreProvider value={createAppStore(bridge)}><PolicySettings /></StoreProvider>);
  await user.type(await screen.findByRole("textbox", { name: /Finans yolları/ }), "/example/finance");
  await user.type(screen.getByRole("textbox", { name: /Korumalı yollar/ }), "/example/protected");
  const days = screen.getByRole("spinbutton", { name: /saklama süresi/ });
  await user.clear(days);
  await user.type(days, "45");
  await user.click(screen.getByRole("button", { name: "Kuralları kaydet" }));
  await screen.findByText("Kurallar kaydedildi.");
  expect(await bridge.getPolicyConfig()).toEqual({ config: { ...policySettings.config, finance_paths: ["/example/finance"], protected_paths: ["/example/protected"] }, trash_retention_days: 45 });
});

it("trash_requires_confirmation_and_returns_restore_destination", async () => {
  const bridge = createFakeBridge({ trash: [{ id: "example-trash", original_path: "/example/file.txt", deleted_at: new Date().toISOString(), chat_id: null, size: 8, is_dir: false }] });
  const user = userEvent.setup();
  render(<StoreProvider value={createAppStore(bridge)}><TrashSettings /></StoreProvider>);
  await user.click(await screen.findByRole("button", { name: "Kalıcı sil" }));
  const dialog = await screen.findByRole("dialog", { name: "Kalıcı silmeyi onayla" });
  expect(bridge.calls.some((call) => call.cmd === "purge_trash")).toBe(false);
  await user.click(within(dialog).getByRole("button", { name: "Vazgeç" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  await user.click(screen.getByRole("button", { name: "Geri yükle" }));
  await screen.findByText("Geri yüklendi: /example/file.txt");
  expect((await bridge.listTrash()).entries).toHaveLength(0);
});

it("trash_timeout_shows_uncertain_result_and_relists_without_repeating_mutation", async () => {
  const entry = { id: "example-trash", original_path: "/example/file.txt", deleted_at: new Date().toISOString(), chat_id: null, size: 8, is_dir: false };
  const bridge = createFakeBridge({ trash: [entry] });
  const real = bridge.restoreTrash;
  let attempts = 0;
  bridge.restoreTrash = async (id) => {
    attempts++;
    await real(id); // daemon uyguladı ama yanıt kayboldu
    throw { kind: "protocol", message: "sonuç belirsiz: yanıt alınamadı; işlem uygulanmış olabilir, çöp kutusu listesi yenileniyor" };
  };
  const user = userEvent.setup();
  render(<StoreProvider value={createAppStore(bridge)}><TrashSettings /></StoreProvider>);
  await user.click(await screen.findByRole("button", { name: "Geri yükle" }));
  await screen.findByText(/Sonuç belirsiz/);
  await screen.findByText("Çöp kutusu boş");
  expect(attempts).toBe(1);
  expect(bridge.calls.filter((call) => call.cmd === "list_trash").length).toBeGreaterThanOrEqual(2);
});

it("policy_violation_is_in_the_timeline_without_stopping_the_turn", () => {
  const timeline = createChatTimeline();
  const snapshot = timeline.append([
    { id: "example-start", chat_id: "example", session_id: null, seq: 1, ts: new Date().toISOString(), kind: "session_started", payload: {} },
    { id: "example-violation", chat_id: "example", session_id: null, seq: 2, ts: new Date().toISOString(), kind: "policy_violation", payload: { command: "rm example.txt", category: "Delete" } },
  ]);
  expect(snapshot.rows.at(-1)).toMatchObject({ type: "system", tone: "policy", text: expect.stringContaining("rm example.txt") });
  expect(snapshot.running).toBe(true);
});
