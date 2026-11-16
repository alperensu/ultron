import type { ReactNode } from "react";

/** Boş durum: ne olduğunu dürüstçe söyler, gerekirse bir eylem sunar. */
export function EmptyState({
  title,
  description,
  action,
}: {
  title: string;
  description?: string;
  action?: ReactNode;
}) {
  return (
    <div className="ui-empty">
      <h2>{title}</h2>
      {description ? <p>{description}</p> : null}
      {action}
    </div>
  );
}
