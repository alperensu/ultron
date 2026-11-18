import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { ShadowScore } from "./ShadowScore";

it("shows the shadow score as a percentage line", () => {
  render(<ShadowScore score={{ p_safe: 0.874 }} />);
  expect(screen.getByText("yerel model: %87 güvenli (gölge)")).toBeInTheDocument();
});

it("rounds the edges correctly", () => {
  const { rerender } = render(<ShadowScore score={{ p_safe: 1 }} />);
  expect(screen.getByText("yerel model: %100 güvenli (gölge)")).toBeInTheDocument();
  rerender(<ShadowScore score={{ p_safe: 0 }} />);
  expect(screen.getByText("yerel model: %0 güvenli (gölge)")).toBeInTheDocument();
});

it("renders nothing when no score exists", () => {
  const { container, rerender } = render(<ShadowScore score={null} />);
  expect(container).toBeEmptyDOMElement();
  rerender(<ShadowScore />);
  expect(container).toBeEmptyDOMElement();
});

it("renders nothing for out-of-range or non-finite values instead of inventing a number", () => {
  const { container, rerender } = render(<ShadowScore score={{ p_safe: Number.NaN }} />);
  expect(container).toBeEmptyDOMElement();
  rerender(<ShadowScore score={{ p_safe: 1.2 }} />);
  expect(container).toBeEmptyDOMElement();
});

it("has no buttons and does not change any decision controls", () => {
  render(<ShadowScore score={{ p_safe: 0.5 }} />);
  expect(screen.queryByRole("button")).toBeNull();
});
