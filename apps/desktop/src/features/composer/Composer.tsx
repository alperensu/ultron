import { motion } from "motion/react";
import { useRef, useState, type KeyboardEvent } from "react";
import { describeBridgeError, type RouteResult } from "../../lib/bridge";
import { navigate, chatPath } from "../../state/router";
import { useAppState, useStore } from "../../state/store";
import { Icon } from "../../ui";
import { spring } from "../../design/motion";
import { ProjectPicker } from "./ProjectPicker";
import { isAmbiguous, useRouting } from "./useRouting";
import "./composer.css";

/** Birleşik eleman: tek kutudan gönderilen mesaj sohbet ekranındaki balonla aynı `layoutId`yi taşır. */
export const SENT_MESSAGE_LAYOUT_ID = "sent-message";

/**
 * Tek kutu: yazarken proje yönlendirilir ("→ Proje (otomatik)"), çipe basınca proje seçilir,
 * güven düşükse ve alternatif varsa göndermeden önce tek satır sorulur.
 */
export function Composer({ chatId, currentProjectId, disabled = false }: {
  chatId?: string;
  currentProjectId?: string | null;
  disabled?: boolean;
}) {
  const store = useStore();
  const projects = useAppState((s) => s.projects);
  const [text, setText] = useState("");
  /** Elle seçim: `undefined` otomatik, `null` projesiz, metin proje kimliği. */
  const [picked, setPicked] = useState<string | null | undefined>(undefined);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [question, setQuestion] = useState<RouteResult | null>(null);
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const fieldRef = useRef<HTMLTextAreaElement>(null);
  const { route, resolve } = useRouting(store.bridge, chatId ? "" : text);

  const projectName = (id: string | null | undefined) =>
    id ? (projects.find((p) => p.id === id)?.name ?? id) : null;

  async function send(projectId: string | null): Promise<void> {
    setSending(true);
    setError(null);
    try {
      if (chatId) {
        await store.bridge.sendMessage({
          chat_id: chatId,
          text: text.trim(),
          ...(currentProjectId ? { project_id: currentProjectId } : {}),
        });
        setText("");
        return;
      }
      const newChatId = await store.createChat(text.trim(), projectId);
      if (projectId) store.expandProject(projectId);
      setText("");
      setPicked(undefined);
      setQuestion(null);
      navigate(chatPath(newChatId));
    } catch (err) {
      setError(describeBridgeError(err));
    } finally {
      setSending(false);
    }
  }

  async function submit(): Promise<void> {
    if (text.trim() === "" || sending || disabled) return;
    if (chatId) return send(null);
    if (picked !== undefined) return send(picked);
    const result = await resolve();
    if (result && isAmbiguous(result)) {
      setQuestion(result);
      return;
    }
    return send(result?.project_id ?? null);
  }

  function onKeyDown(event: KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault();
      void submit();
    }
  }

  function onInput(value: string) {
    setText(value);
    setQuestion(null);
    const field = fieldRef.current;
    if (field) {
      field.style.height = "auto";
      field.style.height = `${Math.min(field.scrollHeight, 120)}px`;
    }
  }

  const hasText = text.trim() !== "";
  const chosen = picked !== undefined ? picked : (route?.project_id ?? null);
  const auto = picked === undefined && route !== null;
  const chipLabel = !hasText || chatId
    ? null
    : chosen
      ? `→ ${projectName(chosen)}${auto ? " (otomatik)" : ""}`
      : picked === null || auto
        ? `→ Projesiz${auto ? " (otomatik)" : ""}`
        : "Proje seç";

  const candidates = question
    ? [question.project_id, ...(question.alternatives ?? [])].filter((id): id is string => id !== null)
    : [];

  return (
    <div className="composer">
      {question ? (
        <div className="composer__ask" role="group" aria-label="Proje sorusu">
          <span>Hangi projeye gitsin?</span>
          {candidates.map((id) => (
            <button key={id} type="button" className="composer__option press" onClick={() => void send(id)}>
              {projectName(id)}
            </button>
          ))}
          <button type="button" className="composer__option press" onClick={() => void send(null)}>
            Projesiz
          </button>
        </div>
      ) : null}
      {error ? (
        <p className="composer__error" role="alert">
          {error}
        </p>
      ) : null}
      <div className="composer__row">
        <motion.div layoutId={chatId ? undefined : SENT_MESSAGE_LAYOUT_ID} transition={spring(0.5)} className="composer__field">
          <textarea
            ref={fieldRef}
            rows={1}
            value={text}
            placeholder={chatId ? "Bu sohbete yaz" : "Ne yapmak istersin?"}
            disabled={sending}
            aria-label="Mesaj"
            onChange={(e) => onInput(e.target.value)}
            onKeyDown={onKeyDown}
          />
          {chipLabel ? (
            <button
              type="button"
              className="composer__chip press"
              aria-label={`Proje: ${chipLabel}. Değiştirmek için seç`}
              onClick={() => setPickerOpen(true)}
            >
              {chipLabel}
            </button>
          ) : null}
        </motion.div>
        <button
          type="button"
          className="composer__send press"
          aria-label="Gönder"
          disabled={!hasText || sending || disabled}
          onClick={() => void submit()}
        >
          <Icon name="send" size={18} />
        </button>
      </div>
      <ProjectPicker
        open={pickerOpen}
        projects={projects}
        selectedId={chosen}
        onSelect={(id) => {
          setPicked(id);
          setQuestion(null);
          setPickerOpen(false);
          fieldRef.current?.focus();
        }}
        onClose={() => setPickerOpen(false)}
      />
    </div>
  );
}
