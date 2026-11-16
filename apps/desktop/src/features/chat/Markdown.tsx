import { memo, useState, type ReactNode } from "react";

/** Ham HTML ayrıştırılmaz. URL şeması hem sözdizimi hem URL parser ile sınırlandırılır. */
export function safeHttpUrl(raw: string): string | null {
  if (!/^https?:\/\//i.test(raw) || /\s/.test(raw) || Array.from(raw).some((character) => character.charCodeAt(0) < 32 || character.charCodeAt(0) === 127)) return null;
  try {
    const url = new URL(raw);
    return (url.protocol === "http:" || url.protocol === "https:") && !url.username && !url.password
      ? url.href
      : null;
  } catch {
    return null;
  }
}

function inline(text: string, depth = 0): ReactNode[] {
  if (depth > 2) return [text];
  const pattern = /(`[^`\n]+`|\*\*[^*\n]+\*\*|\*[^*\n]+\*|\[[^[\]\n]+\]\([^()[\]\s<>]*\))/g;
  const nodes: ReactNode[] = [];
  let cursor = 0;
  for (const match of text.matchAll(pattern)) {
    const index = match.index!;
    if (index > cursor) nodes.push(text.slice(cursor, index));
    const token = match[0];
    const key = `${index}`;
    if (token.startsWith("`")) {
      nodes.push(<code key={key}>{token.slice(1, -1)}</code>);
    } else if (token.startsWith("**")) {
      nodes.push(<strong key={key}>{inline(token.slice(2, -2), depth + 1)}</strong>);
    } else if (token.startsWith("*")) {
      nodes.push(<em key={key}>{inline(token.slice(1, -1), depth + 1)}</em>);
    } else {
      const split = token.indexOf("](");
      const label = token.slice(1, split);
      const href = safeHttpUrl(token.slice(split + 2, -1));
      nodes.push(href ? (
        <a key={key} href={href} target="_blank" rel="noopener noreferrer">{label}</a>
      ) : <span key={key}>{label}</span>);
    }
    cursor = index + token.length;
  }
  if (cursor < text.length) nodes.push(text.slice(cursor));
  return nodes;
}

function CodeBlock({ code, language }: { code: string; language: string }) {
  const [status, setStatus] = useState<"idle" | "copied" | "failed">("idle");
  async function copy() {
    try {
      await navigator.clipboard.writeText(code);
      setStatus("copied");
    } catch {
      setStatus("failed");
    }
  }
  return (
    <div className="chat-code">
      <div className="chat-code__head">
        <span>{language || "Kod"}</span>
        <button type="button" onClick={() => void copy()}>Kopyala</button>
        <span role="status">{status === "copied" ? "Kopyalandı" : status === "failed" ? "Kopyalanamadı; kodu seçip kopyalayabilirsin." : ""}</span>
      </div>
      <pre><code>{code}</code></pre>
    </div>
  );
}

/** Paragraf, kod, vurgu, liste ve güvenli link alt kümesi; yalnız React elemanları üretir. */
export const Markdown = memo(function Markdown({ text }: { text: string }) {
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  const blocks: ReactNode[] = [];
  let index = 0;
  while (index < lines.length) {
    const line = lines[index]!;
    if (line.trim() === "") { index++; continue; }
    if (/^\s*```/.test(line)) {
      const start = index++;
      const language = line.trim().slice(3).trim();
      const code: string[] = [];
      while (index < lines.length && !/^\s*```\s*$/.test(lines[index]!)) code.push(lines[index++]!);
      if (index < lines.length) index++;
      blocks.push(<CodeBlock key={start} code={code.join("\n")} language={language} />);
      continue;
    }
    const list = /^\s*(?:([-+*])|(\d+)\.)\s+(.+)$/.exec(line);
    if (list) {
      const start = index;
      const ordered = Boolean(list[2]);
      const items: ReactNode[] = [];
      while (index < lines.length) {
        const item = /^\s*(?:([-+*])|(\d+)\.)\s+(.+)$/.exec(lines[index]!);
        if (!item || Boolean(item[2]) !== ordered) break;
        items.push(<li key={index}>{inline(item[3]!)}</li>);
        index++;
      }
      blocks.push(ordered ? <ol key={start}>{items}</ol> : <ul key={start}>{items}</ul>);
      continue;
    }
    const start = index;
    const paragraph: string[] = [lines[index++]!];
    while (index < lines.length && lines[index]!.trim() !== "" && !/^\s*(?:```|[-+*]\s|\d+\.\s)/.test(lines[index]!)) {
      paragraph.push(lines[index++]!);
    }
    blocks.push(<p key={start}>{inline(paragraph.join("\n"))}</p>);
  }
  return <div className="chat-markdown">{blocks}</div>;
});
