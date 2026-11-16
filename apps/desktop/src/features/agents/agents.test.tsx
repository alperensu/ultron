import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { createFakeBridge, type ChatInfo, type StoredEvent } from "../../lib/bridge";
import { createAppStore, StoreProvider } from "../../state/store";
import { AgentList } from "./AgentList";
import { createAgentMessageQueue } from "./messageQueue";
import { deriveAgents, elapsed } from "./model";

const chat: ChatInfo = {
  id: "chat-agents", title: "Örnek: ajanlar", engine: "claude", project_id: null,
  pinned: false, archived: false, updated_at: "2026-01-01T12:00:00Z",
};
function event(seq: number, kind: string, payload: Record<string, unknown> = {}, session: string | null = "session-claude"): StoredEvent {
  return {
    id: `agent-event-${seq}`, chat_id: chat.id, session_id: session, seq,
    ts: new Date(Date.UTC(2026, 0, 1, 12, 0, seq)).toISOString(), kind, payload: { type: kind, ...payload },
  };
}
const sends = (bridge: ReturnType<typeof createFakeBridge>) => bridge.calls.filter((call) => call.cmd === "send_message");

describe("ajan oturumları", () => {
  it("agent_list_derives_sessions_and_subagents", () => {
    const events = [
      event(1, "session_started"),
      event(2, "tool_call", { tool: "Task", input: { description: "Örnek test taraması" } }),
      event(3, "tool_call", { tool: "Agent", input: { description: "Örnek belge taraması" } }),
      event(4, "tool_result", { tool: "Agent", is_error: true, output_excerpt: "Örnek belge bulunamadı" }),
      event(5, "tool_result", { tool: "Read", is_error: false, output_excerpt: "Başka araç" }),
      event(6, "tool_result", { tool: "Task", is_error: false, output_excerpt: "Örnek testler geçti" }),
      event(7, "turn_completed"),
      event(8, "engine_switched", { from: "claude", to: "codex" }),
      event(9, "session_started", {}, "session-codex"),
      event(10, "tool_call", { tool: "Task", input: { description: "Claude alt ajanı değil" } }, "session-codex"),
      event(11, "assistant_text", { text: "İlk satır\nsonraki satır" }, "session-codex"),
      { ...event(12, "session_started"), chat_id: "other-chat" },
    ];
    const agents = deriveAgents([...events.slice().reverse(), events[1]!], chat.id, "codex");
    expect(agents).toHaveLength(4);
    expect(agents[0]).toMatchObject({ engine: "claude", status: "closed" });
    expect(agents[1]).toMatchObject({ title: "Örnek test taraması", status: "completed", step: "Örnek testler geçti" });
    expect(agents[1]!.events.map((item) => item.seq)).toEqual([2, 6]);
    expect(agents[2]).toMatchObject({ title: "Örnek belge taraması", status: "error", step: "Örnek belge bulunamadı" });
    expect(agents[3]).toMatchObject({ engine: "codex", status: "running", step: "İlk satır sonraki satır" });
    expect(elapsed(agents[1]!, Date.UTC(2026, 0, 1, 13))).toBe("00:04");
    expect(deriveAgents([event(1, "user_text", {}, null)], chat.id, "claude")).toEqual([]);
  });

  it("does_not_invent_success_for_a_subagent_without_a_result", () => {
    const agents = deriveAgents([
      event(1, "session_started"),
      event(2, "tool_call", { tool: "Task", input: {} }),
      event(3, "turn_completed"),
    ], chat.id, "claude");
    expect(agents[1]).toMatchObject({ status: "unknown", step: "Alt ajan sonucu alınmadı" });
  });
});

describe("ajan mesaj kuyruğu", () => {
  it("queued_message_sent_after_turn_completed", async () => {
    const bridge = createFakeBridge({ chats: [chat], events: [event(1, "session_started")] });
    const queue = createAgentMessageQueue(bridge, chat.id);
    try {
      await queue.enqueue("  Testleri de çalıştır  ", true);
      expect(sends(bridge)).toHaveLength(0);
      expect(queue.getSnapshot()[0]).toMatchObject({ text: "Testleri de çalıştır", status: "queued" });
      bridge.emit({ ...event(2, "turn_completed"), chat_id: "other-chat" });
      bridge.emit(event(3, "tool_result", { tool: "Task", output_excerpt: "Bitti" }));
      expect(sends(bridge)).toHaveLength(0);
      bridge.emit(event(4, "turn_completed"));
      await waitFor(() => expect(queue.getSnapshot()[0]?.status).toBe("sent"));
      expect(sends(bridge)).toEqual([{ cmd: "send_message", args: [{ chat_id: chat.id, text: "Testleri de çalıştır" }] }]);
      bridge.emit(event(4, "turn_completed"));
      expect(sends(bridge)).toHaveLength(1);
    } finally { queue.dispose(); bridge.dispose(); }
  });

  it("waits_for_each_turn_and_keeps_waiting_after_cancel_without_completion", async () => {
    const bridge = createFakeBridge({ chats: [chat], events: [event(1, "session_started")] });
    const queue = createAgentMessageQueue(bridge, chat.id);
    try {
      await queue.enqueue("Birinci", true);
      await queue.enqueue("İkinci", true);
      await bridge.cancelTurn(chat.id);
      expect(sends(bridge)).toHaveLength(0);
      bridge.emit(event(2, "turn_completed"));
      await waitFor(() => expect(queue.getSnapshot()[0]?.status).toBe("sent"));
      expect(sends(bridge)).toHaveLength(1);
      expect(queue.getSnapshot()[1]?.status).toBe("queued");
      bridge.emit(event(3, "turn_completed"));
      await waitFor(() => expect(queue.getSnapshot()[1]?.status).toBe("sent"));
      expect(sends(bridge)).toHaveLength(2);
    } finally { queue.dispose(); bridge.dispose(); }
  });

  it("does_not_lose_completion_while_subscription_resolves", async () => {
    const bridge = createFakeBridge({ chats: [chat], events: [event(1, "session_started")] });
    const original = bridge.onEvent;
    bridge.onEvent = async (handler) => {
      const off = await original(handler);
      bridge.emit(event(2, "turn_completed"));
      return off;
    };
    const queue = createAgentMessageQueue(bridge, chat.id);
    try {
      await queue.enqueue("Yarışta kaybolmaz", true);
      await waitFor(() => expect(queue.getSnapshot()[0]?.status).toBe("sent"));
      expect(sends(bridge)).toHaveLength(1);
    } finally { queue.dispose(); bridge.dispose(); }
  });

  it("keeps_failed_messages_visible_without_automatic_resending", async () => {
    const bridge = createFakeBridge({ chats: [chat] });
    const send = vi.spyOn(bridge, "sendMessage").mockRejectedValue({ kind: "daemon", message: "Örnek gönderim hatası" });
    const queue = createAgentMessageQueue(bridge, chat.id);
    try {
      await queue.enqueue("Kaybolmayan mesaj", false);
      await waitFor(() => expect(queue.getSnapshot()[0]).toMatchObject({ status: "failed", error: "Örnek gönderim hatası" }));
      bridge.emit(event(10, "turn_completed"));
      expect(send).toHaveBeenCalledTimes(1);
    } finally { queue.dispose(); bridge.dispose(); }
  });

  it("uses_live_completion_when_the_rendered_running_flag_is_one_frame_behind", async () => {
    const bridge = createFakeBridge({ chats: [chat], events: [event(1, "session_started")] });
    const queue = createAgentMessageQueue(bridge, chat.id);
    const disconnect = queue.connect();
    try {
      await Promise.resolve();
      bridge.emit(event(2, "turn_completed"));
      await queue.enqueue("Yeni kareyi beklemez", true, 1);
      await waitFor(() => expect(queue.getSnapshot()[0]?.status).toBe("sent"));
      expect(sends(bridge)).toHaveLength(1);
    } finally { disconnect(); queue.dispose(); bridge.dispose(); }
  });

  it("retries_only_explicit_busy_rejections_while_the_completed_turn_unlocks", async () => {
    const bridge = createFakeBridge({ chats: [chat] });
    const send = vi.spyOn(bridge, "sendMessage").mockRejectedValueOnce({ kind: "daemon", message: "sohbette çalışan bir tur var" });
    const queue = createAgentMessageQueue(bridge, chat.id);
    try {
      await queue.enqueue("Kilit bırakılınca gönder", false);
      await waitFor(() => expect(queue.getSnapshot()[0]?.status).toBe("sent"));
      expect(send).toHaveBeenCalledTimes(2);
      expect(sends(bridge)).toHaveLength(1);
    } finally { queue.dispose(); bridge.dispose(); }
  });
});

it("agent_detail_back_restores_focus", async () => {
  const bridge = createFakeBridge({ chats: [chat], events: [event(1, "session_started")] });
  const { container, unmount } = render(<StoreProvider value={createAppStore(bridge)}>
    <input aria-label="Sohbet mesajı" />
    <AgentList chatId={chat.id} />
  </StoreProvider>);
  try {
    const user = userEvent.setup();
    const row = await screen.findByRole("button", { name: /Claude oturumu/ });
    await user.click(row);
    const detail = screen.getByRole("region", { name: "Claude oturumu" });
    expect(container).toContainElement(detail);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const back = within(detail).getByRole("button", { name: "‹ Ajanlar" });
    expect(back).toHaveFocus();
    await user.click(screen.getByRole("textbox", { name: "Sohbet mesajı" }));
    expect(screen.getByRole("textbox", { name: "Sohbet mesajı" })).toHaveFocus();
    await user.click(back);
    expect(screen.queryByRole("region", { name: "Claude oturumu" })).not.toBeInTheDocument();
    expect(row).toHaveFocus();
    await user.keyboard("{Enter}");
    expect(screen.getByRole("button", { name: "‹ Ajanlar" })).toHaveFocus();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("region", { name: "Claude oturumu" })).not.toBeInTheDocument();
    expect(row).toHaveFocus();
  } finally { unmount(); bridge.dispose(); }
});

it("agent_detail_keeps_queued_message_after_back", async () => {
  const bridge = createFakeBridge({ chats: [chat], events: [event(1, "session_started")] });
  const { unmount } = render(<StoreProvider value={createAppStore(bridge)}><AgentList chatId={chat.id} /></StoreProvider>);
  try {
    const user = userEvent.setup();
    const row = await screen.findByRole("button", { name: /Claude oturumu/ });
    await user.click(row);
    await user.type(screen.getByRole("textbox", { name: "Ajana mesaj yaz" }), "Sohbete devam et");
    await user.click(screen.getByRole("button", { name: "Kuyruğa al" }));
    expect(await screen.findByText("tur bitince iletilecek")).toBeInTheDocument();
    expect(sends(bridge)).toHaveLength(0);
    await user.click(screen.getByRole("button", { name: "‹ Ajanlar" }));
    expect(row).toHaveFocus();
    await user.click(row);
    expect(await screen.findByText("tur bitince iletilecek")).toBeInTheDocument();
    unmount();
    act(() => bridge.emit(event(2, "turn_completed")));
    await waitFor(() => expect(sends(bridge)).toHaveLength(1));
  } finally { unmount(); bridge.dispose(); }
});
