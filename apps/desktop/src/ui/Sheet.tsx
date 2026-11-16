import { AnimatePresence, motion } from "motion/react";
import { useEffect, useId, useRef, type ReactNode } from "react";
import { reducedMotion, spring } from "../design/motion";

const FOCUSABLE =
  'a[href],button:not([disabled]),input:not([disabled]),textarea:not([disabled]),select:not([disabled]),[tabindex]:not([tabindex="-1"])';

/**
 * Modal sayfa: alttan ("bottom") ya da sağdan ("right") kayar. Odağı içeride tutar (Tab döngüsü),
 * Escape ve arka plana tıklama kapatır, kapanınca odağı önceki öğeye geri verir.
 */
export function Sheet({
  open,
  onClose,
  title,
  side = "bottom",
  children,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  side?: "bottom" | "right";
  children: ReactNode;
}) {
  return (
    <AnimatePresence>
      {open ? (
        <SheetInner key="sheet" onClose={onClose} title={title} side={side}>
          {children}
        </SheetInner>
      ) : null}
    </AnimatePresence>
  );
}

function SheetInner({
  onClose,
  title,
  side,
  children,
}: {
  onClose: () => void;
  title: string;
  side: "bottom" | "right";
  children: ReactNode;
}) {
  const panelRef = useRef<HTMLDivElement>(null);
  const titleId = useId();

  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const panel = panelRef.current;
    const first = panel?.querySelector<HTMLElement>(FOCUSABLE);
    (first ?? panel)?.focus();

    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
        return;
      }
      if (event.key !== "Tab" || !panel) return;
      const items = Array.from(panel.querySelectorAll<HTMLElement>(FOCUSABLE));
      if (items.length === 0) {
        event.preventDefault();
        panel.focus();
        return;
      }
      const firstEl = items[0]!;
      const lastEl = items[items.length - 1]!;
      const active = document.activeElement;
      if (event.shiftKey && (active === firstEl || !panel.contains(active))) {
        event.preventDefault();
        lastEl.focus();
      } else if (!event.shiftKey && (active === lastEl || !panel.contains(active))) {
        event.preventDefault();
        firstEl.focus();
      }
    }
    document.addEventListener("keydown", onKey, true);
    return () => {
      document.removeEventListener("keydown", onKey, true);
      previous?.focus?.();
    };
  }, [onClose]);

  const hidden = reducedMotion()
    ? { opacity: 0 }
    : side === "bottom"
      ? { opacity: 1, y: "101%" }
      : { opacity: 1, x: "104%" };
  const shown = side === "bottom" ? { opacity: 1, y: 0 } : { opacity: 1, x: 0 };

  return (
    <>
      <motion.div
        className="ui-scrim"
        initial={{ opacity: 0 }}
        animate={{ opacity: 1 }}
        exit={{ opacity: 0 }}
        transition={spring()}
        onClick={onClose}
        data-testid="sheet-scrim"
      />
      <motion.div
        ref={panelRef}
        className={`ui-sheet ui-sheet--${side}`}
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        tabIndex={-1}
        initial={hidden}
        animate={shown}
        exit={hidden}
        transition={spring()}
      >
        {side === "bottom" ? <div className="ui-sheet__grab" aria-hidden="true" /> : null}
        <div className="ui-sheet__head">
          <h3 id={titleId}>{title}</h3>
        </div>
        <div className="ui-sheet__body">{children}</div>
      </motion.div>
    </>
  );
}
