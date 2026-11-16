import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { createFakeBridge, type ProjectInfo, type RouteResult } from "../../lib/bridge";
import { createAppStore, StoreProvider } from "../../state/store";
import { Composer } from "./Composer";

const project = (id: string, name: string): ProjectInfo => ({
  id,
  name,
  path: `/sentetik/${id}`,
  aliases: [],
  description: null,
  last_active_at: null,
});

function setup(route: (text: string) => RouteResult) {
  const bridge = createFakeBridge({
    projects: [project("p1", "Alfa"), project("p2", "Beta")],
    route,
  });
  const store = createAppStore(bridge);
  const stop = store.start();
  render(
    <StoreProvider value={store}>
      <Composer />
    </StoreProvider>,
  );
  return { bridge, stop };
}

const sends = (bridge: ReturnType<typeof createFakeBridge>) =>
  bridge.calls.filter((c) => c.cmd === "send_message");

describe("Composer", () => {
  it("composer shows routed project chip", async () => {
    const { bridge, stop } = setup(() => ({
      project_id: "p1",
      confidence: 0.92,
      reason: "ad eşleşti",
      alternatives: [],
    }));
    await userEvent.type(screen.getByLabelText("Mesaj"), "alfa için plan yaz");

    // 300 ms gecikmeden sonra tek istek atılır ve çip proje adını "(otomatik)" ile gösterir.
    expect(await screen.findByText("→ Alfa (otomatik)")).toBeInTheDocument();
    expect(bridge.calls.filter((c) => c.cmd === "route_message")).toHaveLength(1);
    stop();
  });

  it("ambiguous route asks before sending", async () => {
    const { bridge, stop } = setup(() => ({
      project_id: "p1",
      confidence: 0.4,
      reason: "belirsiz",
      alternatives: ["p2"],
    }));
    const field = screen.getByLabelText("Mesaj");
    await userEvent.type(field, "bir şeyler yap");
    await screen.findByText(/→ Alfa/);
    await userEvent.type(field, "{Enter}");

    // Soru çıkar, henüz gönderilmez.
    expect(await screen.findByText("Hangi projeye gitsin?")).toBeInTheDocument();
    expect(sends(bridge)).toHaveLength(0);

    await userEvent.click(screen.getByRole("button", { name: "Beta" }));
    await waitFor(() => expect(sends(bridge)).toHaveLength(1));
    expect(sends(bridge)[0]!.args[0]).toMatchObject({ text: "bir şeyler yap", project_id: "p2" });
    stop();
  });
});
