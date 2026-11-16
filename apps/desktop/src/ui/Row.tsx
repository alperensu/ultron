import type { CSSProperties, ReactNode } from "react";
import { Icon } from "./icons";

/** Liste satırı. `onClick` verilirse düğmedir (basınca küçülür, ok gösterir); yoksa durağandır. */
export function Row({
  title,
  subtitle,
  meta,
  leading,
  trailing,
  tone,
  index,
  onClick,
}: {
  title: ReactNode;
  subtitle?: ReactNode;
  meta?: ReactNode;
  leading?: ReactNode;
  trailing?: ReactNode;
  tone?: "blue" | "red" | "green" | "orange";
  /** Giriş animasyonu için kademe sırası. */
  index?: number;
  onClick?: () => void;
}) {
  const style = index !== undefined ? ({ "--i": index } as CSSProperties) : undefined;
  const body = (
    <>
      {leading}
      <span className={`ui-row__body${tone ? ` tone-${tone}` : ""}`}>
        {title}
        {subtitle ? <span className="ui-row__sub">{subtitle}</span> : null}
      </span>
      {meta ? <span className="ui-row__meta">{meta}</span> : null}
      {trailing}
      {onClick && !trailing ? <Icon name="chevron" size={12} className="ui-row__chev" /> : null}
    </>
  );
  const cls = `ui-row${index !== undefined ? " ui-rise" : ""}`;
  if (onClick) {
    return (
      <button type="button" className={`${cls} press`} style={style} onClick={onClick}>
        {body}
      </button>
    );
  }
  return (
    <div className={cls} style={style}>
      {body}
    </div>
  );
}
