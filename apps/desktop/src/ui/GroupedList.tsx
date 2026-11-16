import type { ReactNode } from "react";

export function GroupedList({
  header,
  footer,
  children,
}: {
  header?: string;
  footer?: string;
  children: ReactNode;
}) {
  return (
    <section className="ui-section">
      {header ? <h2 className="ui-section__header">{header}</h2> : null}
      <div className="ui-group">{children}</div>
      {footer ? <p className="ui-section__footer">{footer}</p> : null}
    </section>
  );
}
