import { memo, useState } from "react";
import { type ChatRow, workEventSummary } from "./model";

const LINE_HEIGHT = 30;

export const WorkSummary = memo(function WorkSummary({ row }: { row: Extract<ChatRow, { type: "work" }> }) {
  const [scrollTop, setScrollTop] = useState(0);
  const duration = Math.max(0, Date.parse(row.finishedAt) - Date.parse(row.startedAt));
  const minutes = Number.isFinite(duration) ? Math.floor(duration / 60_000) : 0;
  const first = Math.max(0, Math.floor(scrollTop / LINE_HEIGHT) - 3);
  const shown = row.events.slice(first, first + 16);
  return (
    <details className="chat-work">
      <summary>
        <span>{minutes} dk boyunca çalıştı</span>
        <span className="chat-work__count">{row.events.length} işlem</span>
        <span className="chat-work__chevron" aria-hidden="true">›</span>
      </summary>
      <div className="chat-work__scroll" onScroll={(event) => setScrollTop(event.currentTarget.scrollTop)}>
        <ul className="chat-work__items" style={{ height: row.events.length * LINE_HEIGHT }} aria-label="Çalışma ayrıntıları">
          {shown.map((event, index) => (
            <li key={event.id} style={{ top: (first + index) * LINE_HEIGHT, height: LINE_HEIGHT }} title={workEventSummary(event)}>
              {workEventSummary(event)}
            </li>
          ))}
        </ul>
      </div>
    </details>
  );
});
