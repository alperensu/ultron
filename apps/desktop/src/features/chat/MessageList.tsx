import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { motion } from "motion/react";
import { spring } from "../../design/motion";
import { CapsuleButton } from "../../ui";
import { SENT_MESSAGE_LAYOUT_ID } from "../composer/Composer";
import { Markdown } from "./Markdown";
import type { ChatRow } from "./model";
import { WorkSummary } from "./WorkSummary";
import { InlineApproval } from "../approvals/InlineApproval";

const OVERSCAN = 480;

function estimate(row: ChatRow): number {
  if (row.type === "work") return 62;
  if (row.type === "system") return 64;
  if (row.type === "approval") return 280;
  return Math.min(800, 46 + Math.ceil(row.text.length / 65) * 22);
}

/** İlk alt sınırı bulur; her kaydırmada tüm sohbet taranmaz. */
function lowerBound(offsets: number[], value: number): number {
  let low = 0;
  let high = offsets.length - 1;
  while (low < high) {
    const mid = (low + high) >>> 1;
    if (offsets[mid]! < value) low = mid + 1;
    else high = mid;
  }
  return low;
}

export function MessageList({ rows, version, loading, header, sharedMessageId, onProgress }: {
  rows: ChatRow[];
  version: number;
  loading: boolean;
  header: ReactNode;
  sharedMessageId?: string;
  onProgress: (progress: number) => void;
}) {
  const scroller = useRef<HTMLDivElement>(null);
  const headerRef = useRef<HTMLDivElement>(null);
  const heights = useRef(new Map<string, number>());
  const pinned = useRef(true);
  const anchor = useRef<{ index: number; offset: number } | null>(null);
  const [measurement, setMeasurement] = useState(0);
  const [viewport, setViewport] = useState({ top: 0, height: 600, header: 100 });
  const [unread, setUnread] = useState(false);
  const previousVersion = useRef(version);

  const layout = useMemo(() => {
    // Revision okunarak ölçüm önbelleğinin değişimi hesaplamaya dahil edilir.
    void measurement;
    const offsets = [0];
    for (const row of rows) offsets.push(offsets[offsets.length - 1]! + (heights.current.get(row.id) ?? estimate(row)));
    return { offsets, total: offsets[offsets.length - 1]! };
    // measurement is a revision of the measured height cache.
  }, [rows, measurement]);
  const currentLayout = useRef(layout);
  currentLayout.current = layout;

  const measure = useCallback((id: string, height: number) => {
    if (Math.abs((heights.current.get(id) ?? 0) - height) < 1) return;
    heights.current.set(id, height);
    setMeasurement((value) => value + 1);
  }, []);

  useLayoutEffect(() => {
    const element = scroller.current;
    if (!element) return;
    const observer = typeof ResizeObserver === "function" ? new ResizeObserver(() => {
      setViewport((value) => ({ ...value, height: element.clientHeight, header: headerRef.current?.offsetHeight ?? 0 }));
    }) : null;
    observer?.observe(element);
    if (headerRef.current) observer?.observe(headerRef.current);
    setViewport((value) => ({ ...value, height: element.clientHeight, header: headerRef.current?.offsetHeight ?? 0 }));
    return () => observer?.disconnect();
  }, []);

  useLayoutEffect(() => {
    const element = scroller.current;
    if (!element) return;
    if (pinned.current && !loading) {
      element.scrollTop = element.scrollHeight;
    } else if (!pinned.current && anchor.current) {
      // Ölçülen yükseklikler değişince okunan satır aynı piksel konumunda kalır.
      element.scrollTop = viewport.header + (layout.offsets[anchor.current.index] ?? 0) + anchor.current.offset;
    }
    const top = element.scrollTop;
    setViewport((value) => value.top === top ? value : { ...value, top });
  }, [layout, loading, viewport.header]);

  useEffect(() => {
    if (version > previousVersion.current && !pinned.current && !loading) setUnread(true);
    previousVersion.current = version;
  }, [version, loading]);

  function onScroll() {
    const element = scroller.current;
    if (!element) return;
    pinned.current = element.scrollHeight - element.clientHeight - element.scrollTop < 48;
    if (pinned.current) setUnread(false);
    const top = element.scrollTop;
    const listTop = Math.max(0, top - viewport.header);
    const index = Math.max(0, lowerBound(currentLayout.current.offsets, listTop) - 1);
    anchor.current = { index, offset: top - viewport.header - currentLayout.current.offsets[index]! };
    setViewport((value) => ({ ...value, top }));
    onProgress(Math.min(1, top / 56));
  }

  function jumpToLatest() {
    pinned.current = true;
    setUnread(false);
    if (scroller.current) scroller.current.scrollTop = scroller.current.scrollHeight;
  }

  const first = Math.max(0, lowerBound(layout.offsets, viewport.top - viewport.header - OVERSCAN) - 1);
  const end = Math.min(rows.length, lowerBound(layout.offsets, viewport.top - viewport.header + viewport.height + OVERSCAN) + 1);

  return (
    <div className="chat-thread">
      <div className="chat-thread__scroll" ref={scroller} onScroll={onScroll} data-testid="chat-scroll" role="region" aria-label="Mesaj geçmişi" tabIndex={0}>
        <div className="chat-thread__inner">
          <div ref={headerRef} className="chat-thread__header">{header}</div>
          <div className="chat-messages" role="feed" aria-label="Sohbet mesajları" aria-busy={loading} style={{ height: layout.total }}>
            {rows.slice(first, end).map((row, index) => (
              <MeasuredRow key={row.id} row={row} top={layout.offsets[first + index]!} index={first + index} count={rows.length} measure={measure} shared={sharedMessageId === row.id} />
            ))}
          </div>
        </div>
      </div>
      {unread ? (
        <CapsuleButton className="chat-new-messages" onClick={jumpToLatest}>yeni mesajlar ↓</CapsuleButton>
      ) : null}
    </div>
  );
}

const MeasuredRow = memo(function MeasuredRow({ row, top, index, count, measure, shared }: {
  row: ChatRow;
  top: number;
  index: number;
  count: number;
  measure: (id: string, height: number) => void;
  shared: boolean;
}) {
  const ref = useRef<HTMLElement>(null);
  useLayoutEffect(() => {
    const element = ref.current;
    if (!element) return;
    const update = () => measure(row.id, element.getBoundingClientRect().height);
    update();
    const observer = typeof ResizeObserver === "function" ? new ResizeObserver(update) : null;
    observer?.observe(element);
    return () => observer?.disconnect();
  }, [row.id, measure]);

  return (
    <article ref={ref} className={`chat-message chat-message--${row.type}`} style={{ transform: `translateY(${top}px)` }} aria-posinset={index + 1} aria-setsize={count} aria-label={row.type === "user" ? "Sen" : row.type === "assistant" ? "Asistan" : row.type === "work" ? "Çalışma özeti" : "Sistem"}>
      <MessageContent row={row} shared={shared} />
    </article>
  );
});

const MessageContent = memo(function MessageContent({ row, shared }: { row: ChatRow; shared: boolean }) {
  if (row.type === "approval") return <InlineApproval approvalId={row.approvalId} status={row.status} />;
  if (row.type === "work") return <WorkSummary row={row} />;
  if (row.type === "system") return <p role={row.tone === "policy" ? "status" : undefined} className={`chat-system chat-system--${row.tone}${row.tone === "policy" ? " policy-notice" : ""}`}>{row.text}</p>;
  if (row.type === "user") return shared
    ? <motion.div layoutId={SENT_MESSAGE_LAYOUT_ID} transition={spring(0.5)} className="chat-bubble">{row.text}</motion.div>
    : <div className="chat-bubble">{row.text}</div>;
  return <Markdown text={row.text} />;
});
