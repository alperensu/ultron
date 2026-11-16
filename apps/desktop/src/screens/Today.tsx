import { Composer } from "../features/composer/Composer";
import { ENGINE_LABEL, formatDuration, relativeTime } from "../lib/format";
import type { PoolReport } from "../lib/bridge";
import { chatPath, navigate } from "../state/router";
import { useAppState, type CompletedTurn } from "../state/store";
import { EmptyState, GroupedList, LargeTitle, QuotaRing, Row } from "../ui";
import { Page, poolName } from "./Page";
import { tightestWindow } from "./quota";

/** Gösterilen "Sen yokken" satırı üst sınırı; fazlası sayıyla belirtilir. */
const MAX_TURN_ROWS = 20;

export function summaryLine(turns: number, running: number): string {
  const done = turns === 0 ? "Son 24 saatte biten tur yok" : `Son 24 saatte ${turns} tur bitti`;
  return running > 0 ? `${done}, ${running} tur çalışıyor.` : `${done}.`;
}

export function Today() {
  const turns = useAppState((s) => s.turns);
  const daemon = useAppState((s) => s.daemon);
  const running = daemon.state === "ready" ? daemon.runningTurns : 0;

  return (
    <Page title="Bugün" dock={<Composer />}>
      {(progress) => (
        <>
          <LargeTitle title="Bugün" summary={summaryLine(turns.length, running)} progress={progress} />
          <AwayTurns turns={turns} />
          <QuotaStrip />
        </>
      )}
    </Page>
  );
}

function AwayTurns({ turns }: { turns: CompletedTurn[] }) {
  if (turns.length === 0) {
    return (
      <GroupedList header="Sen yokken">
        <EmptyState
          title="Henüz biten tur yok"
          description="Bir sohbette tur tamamlandığında burada sohbet adı, motor ve süresiyle listelenir."
        />
      </GroupedList>
    );
  }
  const shown = turns.slice(0, MAX_TURN_ROWS);
  return (
    <GroupedList
      header="Sen yokken"
      footer={turns.length > shown.length ? `${turns.length - shown.length} tur daha var.` : undefined}
    >
      {shown.map((turn, index) => (
        <Row
          key={turn.eventId}
          index={index}
          title={turn.chatTitle}
          subtitle={[
            turn.engine ? ENGINE_LABEL[turn.engine] : null,
            turn.durationMs !== null ? formatDuration(turn.durationMs) : null,
          ]
            .filter(Boolean)
            .join(" · ")}
          meta={relativeTime(turn.finishedAt)}
          onClick={() => navigate(chatPath(turn.chatId))}
        />
      ))}
    </GroupedList>
  );
}

function QuotaStrip() {
  const quota = useAppState((s) => s.quota);
  const error = useAppState((s) => s.quotaError);

  if (!quota) {
    return (
      <GroupedList header="Kota">
        <EmptyState
          title="Kota bilgisi yok"
          description={error ?? "Kota okunuyor."}
        />
      </GroupedList>
    );
  }
  return (
    <section className="ui-section">
      <h2 className="ui-section__header">Kota</h2>
      <div className="quota-strip">
        {quota.pools.map((pool) => (
          <QuotaTile key={pool.pool} pool={pool} />
        ))}
      </div>
    </section>
  );
}

function QuotaTile({ pool }: { pool: PoolReport }) {
  const window = tightestWindow(pool);
  const percent = window?.used_pct ?? null;
  const detail =
    window === null
      ? "pencere yok"
      : percent === null
        ? `${window.label} · bilinmiyor`
        : `${window.label} · %${Math.round(percent)}${window.source === "Estimated" ? " · tahmini" : ""}`;
  return (
    <div className="quota-tile">
      {percent === null ? (
        <span className="quota-tile__unknown" role="img" aria-label={`${poolName(pool.pool)} kotası bilinmiyor`}>
          ?
        </span>
      ) : (
        <QuotaRing percent={percent} label={`${poolName(pool.pool)} kotası`} />
      )}
      <div className="quota-tile__text">
        <span className="quota-tile__name">{poolName(pool.pool)}</span>
        <span className="quota-tile__detail">{detail}</span>
      </div>
    </div>
  );
}
