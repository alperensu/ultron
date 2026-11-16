import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import { ErrorBoundary } from "./ErrorBoundary";

it("render hatasını okunur gösterir ve yeniden denemede alt ağacı kurar", async () => {
  const consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
  let failing = true;
  function Content() {
    if (failing) throw new Error("Cannot read properties of undefined (reading 'flatMap') <script>");
    return <p>Ultron hazır</p>;
  }
  try {
    render(<ErrorBoundary><Content /></ErrorBoundary>);
    expect(screen.getByRole("alert")).toHaveTextContent("reading 'flatMap'");
    expect(screen.getByRole("heading", { name: "Ultron görüntülenemedi" })).toBeInTheDocument();
    expect(document.querySelector("script")).toBeNull();
    failing = false;
    await userEvent.setup().click(screen.getByRole("button", { name: "Yeniden dene" }));
    expect(screen.getByText("Ultron hazır")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).toBeNull();
  } finally {
    consoleError.mockRestore();
  }
});
