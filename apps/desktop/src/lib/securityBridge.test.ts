import { expect, it } from "vitest";
import { createFakeBridge, createTauriBridge, type Approval } from "./bridge";
import { vi } from "vitest";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn().mockResolvedValue({}) }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

it("security_commands_use_typed_tauri_arguments_without_credentials", async () => {
  const bridge = createTauriBridge();
  await bridge.listApprovals({ status: "pending", cursor: "example", limit: 32 });
  await bridge.resolveApproval("example", true);
  await bridge.getPolicyConfig();
  const settings = { config: { finance_paths: [], protected_paths: [], deploy_patterns: [], allow_list: [] }, trash_retention_days: 30 };
  await bridge.setPolicyConfig(settings);
  await bridge.listTrash();
  await bridge.restoreTrash("example");
  await bridge.purgeTrash("example");
  const telegramSettings = {
    notify: "away" as const, auto_continue_enabled: false,
    auto_continue_wait_minutes: 45, fallback_order: ["codex" as const],
  };
  await bridge.telegramStatus();
  await bridge.getTelegramSettings();
  const extraFields = { ...telegramSettings, token: "synthetic-extra", allowed_user_ids: [1] };
  await bridge.setTelegramSettings(extraFields);
  await bridge.telegramTest();
  expect(invoke.mock.calls).toEqual([
    ["list_approvals", { args: { status: "pending", cursor: "example", limit: 32 } }],
    ["resolve_approval", { args: { id: "example", approve: true } }],
    ["get_policy_config", undefined], ["set_policy_config", { args: { settings } }],
    ["list_trash", undefined], ["restore_trash", { args: { id: "example" } }], ["purge_trash", { args: { id: "example" } }],
    ["telegram_status", undefined], ["get_telegram_settings", undefined],
    ["set_telegram_settings", { args: telegramSettings }], ["telegram_test", undefined],
  ]);
});

it("fake_approvals_resolve_once_and_return_detached_data", async () => {
  const approval: Approval = { id: "example", chat_id: null, engine: "claude", category: "Delete", reason: "Örnek", action_json: "{}", created_at: new Date().toISOString(), status: "pending", resolved_at: null, resolved_by: null };
  const bridge = createFakeBridge({ approvals: [approval] });
  const first = await bridge.listApprovals();
  first.approvals[0]!.reason = "değiştirildi";
  expect((await bridge.listApprovals()).approvals[0]!.reason).toBe("Örnek");
  await bridge.resolveApproval("example", false);
  await expect(bridge.resolveApproval("example", true)).rejects.toMatchObject({ kind: "daemon" });
  expect((await bridge.listApprovals({ status: "pending" })).approvals).toHaveLength(0);
});
