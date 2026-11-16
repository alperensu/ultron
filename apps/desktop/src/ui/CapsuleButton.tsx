import type { ButtonHTMLAttributes } from "react";

type Variant = "primary" | "text" | "danger";

export function CapsuleButton({
  variant = "primary",
  className,
  type = "button",
  ...rest
}: ButtonHTMLAttributes<HTMLButtonElement> & { variant?: Variant }) {
  return (
    <button
      type={type}
      className={`ui-capsule ui-capsule--${variant} press${className ? ` ${className}` : ""}`}
      {...rest}
    />
  );
}
