import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { enterFrom, reducedMotion, spring, stagger } from "../design/motion";
import { QuotaRing, quotaTone } from "./QuotaRing";
import { Sheet } from "./Sheet";

function mockReducedMotion(matches: boolean) {
  vi.stubGlobal(
    "matchMedia",
    (query: string) => ({
      matches: query.includes("prefers-reduced-motion") ? matches : false,
      media: query,
      addEventListener: () => {},
      removeEventListener: () => {},
    }),
  );
  window.matchMedia = globalThis.matchMedia;
}

afterEach(() => vi.unstubAllGlobals());

describe("QuotaRing", () => {
  it("renders percent and color thresholds", () => {
    expect(quotaTone(0)).toBe("ok");
    expect(quotaTone(79.9)).toBe("ok");
    expect(quotaTone(80)).toBe("warn");
    expect(quotaTone(99)).toBe("warn");
    expect(quotaTone(100)).toBe("crit");

    const { rerender } = render(<QuotaRing percent={42} label="Claude" />);
    let ring = screen.getByRole("img", { name: "Claude: %42" });
    expect(ring).toHaveAttribute("data-tone", "ok");

    rerender(<QuotaRing percent={95} label="Claude" />);
    ring = screen.getByRole("img", { name: "Claude: %95" });
    expect(ring).toHaveAttribute("data-tone", "warn");
    const arc = ring.querySelector(".ui-ring__arc")!;
    const c = 2 * Math.PI * 11;
    expect(Number(arc.getAttribute("stroke-dashoffset"))).toBeCloseTo(c * 0.05, 3);

    rerender(<QuotaRing percent={130} label="Claude" />);
    expect(screen.getByRole("img", { name: "Claude: %100" })).toHaveAttribute("data-tone", "crit");
  });
});

function Harness({ onClose }: { onClose: () => void }) {
  const [open, setOpen] = useState(true);
  return (
    <>
      <button>dışarıda</button>
      <Sheet
        open={open}
        title="Deneme"
        onClose={() => {
          onClose();
          setOpen(false);
        }}
      >
        <button>bir</button>
        <button>iki</button>
      </Sheet>
    </>
  );
}

describe("Sheet", () => {
  it("traps focus and closes on Escape", async () => {
    mockReducedMotion(true);
    const user = userEvent.setup();
    const onClose = vi.fn();
    render(<Harness onClose={onClose} />);

    const dialog = await screen.findByRole("dialog", { name: "Deneme" });
    const one = screen.getByRole("button", { name: "bir" });
    const two = screen.getByRole("button", { name: "iki" });
    expect(one).toHaveFocus();

    await user.tab();
    expect(two).toHaveFocus();
    await user.tab(); // sona gelince başa döner, dışarıdaki düğmeye kaçmaz
    expect(one).toHaveFocus();
    await user.tab({ shift: true });
    expect(two).toHaveFocus();
    expect(dialog).toContainElement(document.activeElement as HTMLElement);

    await user.keyboard("{Escape}");
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});

describe("motion", () => {
  it("reduced motion disables transforms", () => {
    mockReducedMotion(true);
    expect(reducedMotion()).toBe(true);
    expect(spring()).toEqual({ duration: 0 });
    expect(stagger(5)).toBe(0);
    expect(enterFrom(8)).toEqual({ opacity: 0 });
    expect(enterFrom(8)).not.toHaveProperty("y");
  });

  it("uses the spring curve and offsets when motion is allowed", () => {
    mockReducedMotion(false);
    expect(reducedMotion()).toBe(false);
    expect(spring()).toMatchObject({ ease: [0.32, 0.72, 0, 1] });
    expect(stagger(2)).toBeCloseTo(0.06);
    expect(enterFrom(8)).toEqual({ opacity: 0, y: 8 });
  });

  it("Sheet renders without a translate transform under reduced motion", async () => {
    mockReducedMotion(true);
    render(<Harness onClose={() => {}} />);
    const dialog = await screen.findByRole("dialog");
    await act(async () => {});
    expect(dialog.style.transform === "" || dialog.style.transform === "none").toBe(true);
  });
});
