import { useLayoutEffect, useRef, useState, type ReactNode } from "react";

/** Sabit satır boyu, sınırlı DOM ve yukarı kaydırırken korunan konum. */
export function VirtualRows<T>({ rows, height, label, follow = false, render }: {
  rows: T[];
  height: number;
  label: string;
  follow?: boolean;
  render: (row: T, index: number) => ReactNode;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const [top, setTop] = useState(0);
  const [viewport, setViewport] = useState(320);
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element) return;
    const measure = () => setViewport(element.clientHeight || 320);
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  useLayoutEffect(() => {
    const element = ref.current;
    if (follow && pinned.current && element) {
      element.scrollTop = element.scrollHeight;
      setTop(element.scrollTop);
    }
  }, [rows.length, follow]);
  const start = Math.max(0, Math.floor(top / height) - 3);
  const end = Math.min(rows.length, Math.ceil((top + viewport) / height) + 3);
  return (
    <div className="agents-virtual" ref={ref} role="list" tabIndex={0} aria-label={label} onScroll={(event) => {
      const element = event.currentTarget;
      pinned.current = element.scrollHeight - element.scrollTop - element.clientHeight < 40;
      setTop(element.scrollTop);
    }}>
      <div className="agents-virtual__space" style={{ height: rows.length * height }}>
        {rows.slice(start, end).map((row, index) => (
          <div key={start + index} role="listitem" aria-posinset={start + index + 1} aria-setsize={rows.length} className="agents-virtual__row" style={{ top: (start + index) * height, height }}>
            {render(row, start + index)}
          </div>
        ))}
      </div>
    </div>
  );
}
