import type { ReactNode } from "react";
import { Icon, type IconName } from "./icons";

export function Sidebar({
  children,
  footer,
  className,
}: {
  children: ReactNode;
  footer?: ReactNode;
  className?: string;
}) {
  return (
    <aside className={`ui-sidebar${className ? ` ${className}` : ""}`} aria-label="Kenar çubuğu">
      <div className="ui-sidebar__brand" data-tauri-drag-region>
        Ultron
      </div>
      <nav className="ui-sidebar__nav">{children}</nav>
      {footer ? <div className="ui-sidebar__foot">{footer}</div> : null}
    </aside>
  );
}

export function SidebarHeading({ children }: { children: ReactNode }) {
  return <div className="ui-sidebar__heading">{children}</div>;
}

export function SidebarItem({
  icon,
  count,
  active,
  sub,
  onClick,
  children,
}: {
  icon?: IconName;
  count?: number;
  active?: boolean;
  sub?: boolean;
  onClick?: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      className={`ui-sidebar__item press${sub ? " is-sub" : ""}${active ? " is-active" : ""}`}
      aria-current={active ? "page" : undefined}
      onClick={onClick}
    >
      {icon ? <Icon name={icon} size={17} /> : null}
      <span className="ui-sidebar__label">{children}</span>
      {count !== undefined ? <span className="ui-sidebar__count">{count}</span> : null}
    </button>
  );
}
