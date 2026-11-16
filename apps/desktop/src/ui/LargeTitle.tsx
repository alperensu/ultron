import type { CSSProperties } from "react";

/** iOS büyük başlık. `progress` 0..1 kaydırma ilerlemesi (yalnızca transform/opacity). */
export function LargeTitle({
  title,
  summary,
  progress = 0,
}: {
  title: string;
  summary?: string;
  progress?: number;
}) {
  const style = { "--p": progress } as CSSProperties;
  return (
    <div className="ui-large-title" style={style}>
      <h1>{title}</h1>
      {summary ? <p>{summary}</p> : null}
    </div>
  );
}
