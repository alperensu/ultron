import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createFakeBridge, type Bridge, type ChatInfo, type StoredEvent } from "../../lib/bridge";
import { createAppStore, StoreProvider } from "../../state/store";
import { Composer } from "../composer/Composer";
import { Markdown, safeHttpUrl } from "./Markdown";
import { createChatTimeline } from "./model";
import { useChat } from "./useChat";
import { WorkSummary } from "./WorkSummary";

const chat: ChatInfo = {
  id: "chat-test", title: "Örnek sohbet", project_id: null,
  engine: "claude", pinned: false, archived: false, updated_at: "2026-01-01T00:00:00Z",
};

function event(seq: number, kind: string, payload: Record<string, unknown> = {}, minute = seq): StoredEvent {
  return {
    id: `event-${seq}`, chat_id: chat.id, session_id: "session-test", seq,
    ts: new Date(Date.UTC(2026, 0, 1, 0, minute)).toISOString(), kind,
    payload: { type: kind, ...payload },
  };
}

function Probe({ bridge }: { bridge: Bridge }) {
  const state = useChat(bridge, chat.id);
  return (
    <>
      <span data-testid="loading">{String(state.loading)}</span>
      <span data-testid="version">{state.version}</span>
      <span data-testid="error">{state.error}</span>
      <span data-testid="running">{String(state.running)}</span>
      {state.rows.map((row) => row.type === "assistant" ? <article key={row.id} aria-label="Asistan"><Markdown text={row.text} /></article> : null)}
    </>
  );
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.useRealTimers();
});

describe("sohbet akışı", () => {
  it("fragments_coalesce_into_one_bubble", async () => {
    const bridge = createFakeBridge({ chats: [chat] });
    render(<Probe bridge={bridge} />);
    await waitFor(() => expect(screen.getByTestId("loading")).toHaveTextContent("false"));

    // Kareyi elle sür: üç olay geldiğinde kareden önce hiçbir balon çizilmez.
    let callback: FrameRequestCallback | undefined;
    const raf = vi.spyOn(window, "requestAnimationFrame").mockImplementation((next) => { callback = next; return 1; });
    act(() => {
      bridge.emit(event(1, "assistant_text", { text: "Bir " }));
      bridge.emit(event(2, "assistant_text", { text: "araya " }));
      bridge.emit(event(3, "assistant_text", { text: "geldi." }));
    });
    expect(raf).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("article")).not.toBeInTheDocument();
    act(() => callback?.(performance.now()));
    expect(screen.getAllByRole("article", { name: "Asistan" })).toHaveLength(1);
    expect(screen.getByText("Bir araya geldi.")).toBeInTheDocument();
    bridge.dispose();
  });

  it("work_summary_groups_tool_events", async () => {
    const timeline = createChatTimeline();
    const snapshot = timeline.append([
      event(1, "assistant_text", { text: "İnceliyorum." }, 0),
      event(2, "tool_call", { tool: "Read", input: {} }, 1),
      event(3, "tool_result", { tool: "Read", output_excerpt: "Dosya okundu", is_error: false }, 2),
      event(4, "command", { command: "npm test", exit_code: 0 }, 3),
      event(5, "file_changed", { path: "src/example.ts", change: "modified" }, 4),
      event(6, "assistant_text", { text: "Bitti." }, 5),
    ]);
    expect(snapshot.rows.map((row) => row.type)).toEqual(["assistant", "work", "assistant"]);
    const work = snapshot.rows[1]!;
    if (work.type !== "work") throw new Error("Çalışma özeti yok");
    expect(work.events).toHaveLength(4);
    render(<WorkSummary row={work} />);
    await userEvent.click(screen.getByText("3 dk boyunca çalıştı"));
    expect(screen.getByText("npm test · çıkış: 0")).toBeVisible();
    expect(screen.getByText("src/example.ts · modified")).toBeVisible();
  });

  it("history_pages_and_live_overlap_are_deduplicated", async () => {
    const history = Array.from({ length: 1201 }, (_, index) => event(index + 1, "assistant_text", { text: "a" }));
    const bridge = createFakeBridge({ chats: [chat], events: history });
    const original = bridge.eventsPage;
    bridge.eventsPage = async (...args) => {
      const page = await original(...args);
      if (args[1] === null) bridge.emit(event(1202, "assistant_text", { text: "son" }));
      return page;
    };
    render(<Probe bridge={bridge} />);
    await waitFor(() => expect(screen.getByTestId("loading")).toHaveTextContent("false"));
    expect(bridge.calls.filter((call) => call.cmd === "events_page")).toHaveLength(3);
    expect(screen.getByTestId("version")).toHaveTextContent("1202");
    expect(screen.getAllByRole("article")).toHaveLength(1);
    expect(screen.getByRole("article")).toHaveTextContent(`${"a".repeat(1201)}son`);
    bridge.dispose();
  });

  it("non_advancing_page_reports_an_error", async () => {
    const bridge = createFakeBridge({ chats: [chat] });
    bridge.eventsPage = async () => ({ events: [], next_after_seq: null, has_more: true });
    render(<Probe bridge={bridge} />);
    await waitFor(() => expect(screen.getByTestId("error")).toHaveTextContent("sayfa ilerlemiyor"));
    bridge.dispose();
  });

  it("late_subscription_is_cleaned_up_after_unmount", async () => {
    const bridge = createFakeBridge({ chats: [chat] });
    const off = vi.fn();
    let resolve: (value: () => void) => void = () => {};
    bridge.onEvent = () => new Promise((done) => { resolve = done; });
    const view = render(<Probe bridge={bridge} />);
    view.unmount();
    await act(async () => resolve(off));
    expect(off).toHaveBeenCalledOnce();
  });

  it("turn_boundaries_do_not_merge_assistant_bubbles", () => {
    const timeline = createChatTimeline();
    const first = timeline.append([event(1, "assistant_text", { text: "Bir" })]);
    const second = timeline.append([
      event(2, "assistant_text", { text: "inci" }),
      event(3, "turn_completed"),
      event(4, "assistant_text", { text: "İkinci" }),
      event(5, "turn_completed"),
    ]);
    expect(first.rows[0]).toMatchObject({ text: "Bir" });
    expect(second.rows).toHaveLength(2);
    expect(second.running).toBe(false);
  });

  it("cancel_without_terminal_event_clears_when_daemon_is_idle", async () => {
    const bridge = createFakeBridge({ chats: [chat], events: [event(1, "user_text", { text: "Başla" })] });
    render(<Probe bridge={bridge} />);
    await waitFor(() => expect(screen.getByTestId("running")).toHaveTextContent("true"));
    await bridge.cancelTurn(chat.id);
    await waitFor(() => expect(screen.getByTestId("running")).toHaveTextContent("false"), { timeout: 2500 });
    expect((await bridge.eventsPage(chat.id, null, 50)).events).toHaveLength(1);
    bridge.dispose();
  });

  it("engine_and_quota_are_distinct_system_lines", () => {
    const snapshot = createChatTimeline("claude").append([
      event(1, "quota_exhausted", { message: "Örnek kota sınırı", resets_at: null }),
      event(2, "engine_switched", { from: "claude", to: "codex", reason: "quota" }),
    ]);
    expect(snapshot.rows).toEqual([
      expect.objectContaining({ type: "system", tone: "quota", text: "Kota doldu: Örnek kota sınırı" }),
      expect.objectContaining({ type: "system", tone: "engine", text: "Motor değişti: Claude → Codex · kota nedeniyle" }),
    ]);
    expect(snapshot.engine).toBe("codex");
  });

  it("fake_bridge_streams_50_events_per_second_and_stops_on_unsubscribe", async () => {
    vi.useFakeTimers();
    const events = Array.from({ length: 50 }, (_, index) => event(index + 1, "assistant_text", { text: "a" }));
    const bridge = createFakeBridge({ chats: [chat], stream: { events, eventsPerSecond: 50 } });
    const received = vi.fn();
    const off = await bridge.onEvent(received);
    await vi.advanceTimersByTimeAsync(1000);
    expect(received).toHaveBeenCalledTimes(50);
    off();
    await vi.advanceTimersByTimeAsync(1000);
    expect(received).toHaveBeenCalledTimes(50);
    bridge.dispose();
  });

  it("chat_composer_sends_to_current_chat_without_routing", async () => {
    const bridge = createFakeBridge({ chats: [chat] });
    render(<StoreProvider value={createAppStore(bridge)}><Composer chatId={chat.id} /></StoreProvider>);
    await userEvent.type(screen.getByLabelText("Mesaj"), "Bu sohbete gönder{Enter}");
    await waitFor(() => expect(bridge.calls.find((call) => call.cmd === "send_message")?.args[0]).toEqual({ chat_id: chat.id, text: "Bu sohbete gönder" }));
    expect(bridge.calls.some((call) => call.cmd === "route_message")).toBe(false);
    bridge.dispose();
  });
});

describe("güvenli markdown", () => {
  it("markdown_is_sanitized", () => {
    const text = '<script>window.pwned = true</script>\n\n<img src=x onerror="alert(1)">\n\n[tıkla](javascript:alert(1)) [veri](data:text/html,evil) [güvenli](https://example.com/docs)';
    const { container } = render(<Markdown text={text} />);
    expect(container.querySelector("script,img,iframe,svg")).toBeNull();
    expect(container.querySelector("[onerror],[onclick]")).toBeNull();
    const links = screen.getAllByRole("link");
    expect(links).toHaveLength(1);
    expect(links[0]).toHaveAttribute("href", "https://example.com/docs");
    expect(links[0]).toHaveAttribute("rel", "noopener noreferrer");
    expect(container.textContent).toContain("<script>");
    for (const url of ["javascript:alert(1)", "data:text/html,evil", "//example.com", "https://user:pass@example.com", "https://example.com/\nfoo"]) expect(safeHttpUrl(url)).toBeNull();
  });

  it("markdown_subset_and_code_copy_work", async () => {
    const copy = vi.fn().mockResolvedValue(undefined);
    const user = userEvent.setup();
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText: copy } });
    const { container } = render(<Markdown text={'**Kalın** ve *italik* ve `kod`\n\n- Bir\n- İki\n\n1. Sıralı\n\n```ts\nconst örnek = "güvenli";\n```'} />);
    expect(container.querySelector("strong")).toHaveTextContent("Kalın");
    expect(container.querySelector("em")).toHaveTextContent("italik");
    expect(container.querySelectorAll("li")).toHaveLength(3);
    await user.click(screen.getByRole("button", { name: "Kopyala" }));
    expect(copy).toHaveBeenCalledWith('const örnek = "güvenli";');
    expect(screen.getByRole("status")).toHaveTextContent("Kopyalandı");
  });
});
