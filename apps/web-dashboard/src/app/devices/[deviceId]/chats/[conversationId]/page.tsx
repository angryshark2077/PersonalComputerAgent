"use client";

import Link from "next/link";
import { useParams, useSearchParams } from "next/navigation";
import { useCallback, useEffect, useId, useRef, useState } from "react";

import { DashboardShell } from "../../../../../components/dashboard-shell";
import {
  DashboardApiError,
  chatReadStorageKey,
  cloudApiOrigin,
  decodeDashboardRouteParam,
  getCommunicationConversations,
  getCommunicationMessages,
  getCommunicationObjectReadUrl,
  mergeLatestCommunicationMessages,
  messagesReadStorageKey,
  type CommunicationSource,
  type DashboardMessage,
} from "../../../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../../../lib/auth";
import {
  parseSharedChatRecord,
  type SharedChatItem,
  type SharedChatRecord,
} from "../../../../../lib/shared-chat";

export default function ChatMessagesPage() {
  return <CommunicationMessagesPage source="communication.wechat" rootPath="chats" />;
}

export function CommunicationMessagesPage({
  source,
  rootPath,
}: {
  source: CommunicationSource;
  rootPath: "chats" | "messages";
}) {
  const params = useParams<{ deviceId: string; conversationId: string }>();
  const searchParams = useSearchParams();
  const deviceId = decodeDashboardRouteParam(params.deviceId);
  const conversationId = decodeDashboardRouteParam(params.conversationId);
  const conversationPage = Math.max(1, Number.parseInt(searchParams.get("page") ?? "1", 10) || 1);
  const [messages, setMessages] = useState<DashboardMessage[] | null>(null);
  const [displayName, setDisplayName] = useState(conversationId);
  const [conversationAvatarUrl, setConversationAvatarUrl] = useState<string | null>(null);
  const [conversationScope, setConversationScope] = useState<"direct" | "group">("direct");
  const [hasOlderMessages, setHasOlderMessages] = useState(false);
  const [loadingOlderMessages, setLoadingOlderMessages] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const messagePanel = useRef<HTMLElement | null>(null);
  const didInitialScroll = useRef(false);
  const loadingOlderMessagesRef = useRef(false);
  const visibleMessages = messages === null ? null : deduplicateMediaUpgrades(messages);

  useEffect(() => {
    const origin = cloudApiOrigin();
    let active = true;
    let loading = false;
    const refresh = async (initial: boolean) => {
      if (loading) return;
      loading = true;
      try {
        const [latest, conversations] = await Promise.all([
          getCommunicationMessages(window.fetch, origin, deviceId, conversationId, source),
          getCommunicationConversations(window.fetch, origin, deviceId, source, 100, conversationPage),
        ]);
        if (!active) return;
        const conversation = conversations.conversations.find(
          (candidate) => candidate.conversation_id === conversationId,
        );
        setDisplayName(conversation?.display_name ?? conversationId);
        setConversationAvatarUrl(conversation?.avatar_url ?? null);
        setConversationScope(conversation?.scope ?? "direct");
        if (initial) setHasOlderMessages(latest.length === 100);
        setMessages((current) => mergeLatestCommunicationMessages(current, latest));
        window.localStorage.setItem(
          (source === "communication.wechat" ? chatReadStorageKey : messagesReadStorageKey)(deviceId, conversationId),
          conversation?.last_message_at ?? latest[0]?.occurred_at ?? new Date().toISOString(),
        );
        setError(null);
      } catch (cause) {
        if (active) setError(messageFor(cause));
      } finally {
        loading = false;
      }
    };
    void (async () => {
      if ((await getBrowserSession(window.fetch, origin)) === null) {
        redirectToSignIn();
        return;
      }
      await refresh(true);
    })();
    const refreshLatest = () => void refresh(false);
    const interval = window.setInterval(refreshLatest, 15_000);
    window.addEventListener("focus", refreshLatest);
    window.addEventListener("pageshow", refreshLatest);
    return () => {
      active = false;
      window.clearInterval(interval);
      window.removeEventListener("focus", refreshLatest);
      window.removeEventListener("pageshow", refreshLatest);
    };
  }, [conversationId, conversationPage, deviceId, source]);

  useEffect(() => {
    if (messages !== null && !didInitialScroll.current) {
      didInitialScroll.current = true;
      messagePanel.current?.scrollTo({ top: messagePanel.current.scrollHeight });
    }
  }, [messages]);

  const loadOlderMessages = useCallback(async () => {
    const panel = messagePanel.current;
    const oldest = messages?.[0];
    if (panel === null || oldest === undefined || !hasOlderMessages || loadingOlderMessagesRef.current) return;
    loadingOlderMessagesRef.current = true;
    setLoadingOlderMessages(true);
    setError(null);
    const previousHeight = panel.scrollHeight;
    try {
      const older = await getCommunicationMessages(
        window.fetch,
        cloudApiOrigin(),
        deviceId,
        conversationId,
        source,
        100,
        oldest,
      );
      setHasOlderMessages(older.length === 100);
      setMessages((current) => current === null ? older.toReversed() : [...older.toReversed(), ...current]);
      window.requestAnimationFrame(() => {
        const currentPanel = messagePanel.current;
        if (currentPanel !== null) currentPanel.scrollTop = currentPanel.scrollHeight - previousHeight;
      });
    } catch (cause) {
      setError(messageFor(cause));
    } finally {
      loadingOlderMessagesRef.current = false;
      setLoadingOlderMessages(false);
    }
  }, [conversationId, deviceId, hasOlderMessages, messages, source]);

  return (
    <DashboardShell>
      <Link className="back-link" href={`/devices/${encodeURIComponent(deviceId)}/${rootPath}`}>Back to {rootPath === "chats" ? "WeChat" : "Messages"}</Link>
      <section className="page-heading">
        <h1>{displayName}</h1>
        <p className="conversation-id">{conversationId}</p>
      </section>
      {error !== null ? <p role="alert">{error}</p> : null}
      {visibleMessages === null ? <p className="status-note">Loading messages…</p> : (
        <section
          ref={messagePanel}
          className="dashboard-panel message-scroll"
          aria-label="Conversation messages"
          onScroll={(event) => {
            if (event.currentTarget.scrollTop <= 40) void loadOlderMessages();
          }}
        >
          {loadingOlderMessages ? <p className="message-page-status">Loading older messages…</p> : null}
          {!hasOlderMessages && visibleMessages.length > 0
            ? <p className="message-page-status">Beginning of synchronized history</p>
            : null}
          {visibleMessages.length === 0 ? <p className="empty-state">No synchronized messages.</p> : (
            <ol className="message-list">
              {visibleMessages.map((message) => (
                <li className={`message-row is-${message.direction}`} key={message.event_id}>
                  <Avatar
                    name={message.direction === "outgoing" ? "我" : message.sender_display_name}
                    url={message.direction === "incoming"
                      ? message.sender_avatar_url ?? (conversationScope === "direct" ? conversationAvatarUrl : null)
                      : null}
                  />
                  <div className="message-content">
                    {conversationScope === "group"
                      ? <p className="message-sender">{message.sender_display_name}</p>
                      : null}
                    <article className="message-bubble">
                      {message.kind === "text"
                        ? <TextMessage text={message.text} />
                        : <MediaSummary deviceId={deviceId} message={message} />}
                      <p className="message-meta">
                        {message.direction === "incoming" ? "Received" : "Sent"} · {formatTime(message.occurred_at)}
                      </p>
                    </article>
                  </div>
                </li>
              ))}
            </ol>
          )}
        </section>
      )}
    </DashboardShell>
  );
}

function deduplicateMediaUpgrades(messages: DashboardMessage[]): DashboardMessage[] {
  const bestByMessageId = new Map<string, DashboardMessage>();
  for (const message of messages) {
    const current = bestByMessageId.get(message.message_id);
    if (current === undefined || mediaQualityScore(message) > mediaQualityScore(current)) {
      bestByMessageId.set(message.message_id, message);
    }
  }
  return messages.filter((message) => bestByMessageId.get(message.message_id)?.event_id === message.event_id);
}

function mediaQualityScore(message: DashboardMessage): number {
  if (message.kind === "text") {
    return message.text === "[视频] 等待微信保存原始文件" ? 0 : 1;
  }
  return 10 + (message.attachments[0]?.size_bytes ?? 0);
}

function Avatar({ name, url }: { name: string; url: string | null }) {
  return url === null ? (
    <span className="chat-avatar message-avatar is-placeholder" aria-hidden="true">{initial(name)}</span>
  ) : (
    <img className="chat-avatar message-avatar" src={url} alt="" loading="lazy" referrerPolicy="no-referrer" />
  );
}

function initial(name: string): string {
  return Array.from(name.trim())[0]?.toUpperCase() ?? "?";
}

function TextMessage({ text }: { text: string | null }) {
  const sharedChat = parseSharedChatRecord(text);
  if (sharedChat !== null) return <SharedChatMessage record={sharedChat} />;
  const contact = parseContactCard(text);
  if (contact === null) return <p className="message-text">{text}</p>;
  return (
    <section className="shared-contact-card" aria-label="Shared WeChat contact">
      <Avatar name={contact.displayName} url={contact.avatarUrl} />
      <div>
        <p className="shared-contact-name">{contact.displayName}</p>
        <p className="message-text">微信号：{contact.wechatId}</p>
      </div>
    </section>
  );
}

function SharedChatMessage({ record }: { record: SharedChatRecord }) {
  const [open, setOpen] = useState(false);
  const titleId = useId();
  const trigger = useRef<HTMLButtonElement | null>(null);
  const closeButton = useRef<HTMLButtonElement | null>(null);
  useEffect(() => {
    if (!open) return;
    closeButton.current?.focus();
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => {
      window.removeEventListener("keydown", closeOnEscape);
      trigger.current?.focus();
    };
  }, [open]);

  return (
    <>
      <button
        ref={trigger}
        className="shared-chat-card"
        type="button"
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => setOpen(true)}
      >
        <span className="shared-chat-card-title">{record.title}</span>
        <span className="shared-chat-card-preview">
          {record.items.slice(0, 3).map((item) => `${item.sender}: ${previewLine(item)}`).join("\n")}
        </span>
        <span className="shared-chat-card-footer">聊天记录 · {record.items.length} 条 · 点击查看全部</span>
      </button>
      {open ? (
        <div
          className="shared-chat-overlay"
          role="dialog"
          aria-modal="true"
          aria-labelledby={titleId}
          onClick={() => setOpen(false)}
        >
          <section className="shared-chat-dialog" onClick={(event) => event.stopPropagation()}>
            <header className="shared-chat-dialog-header">
              <div>
                <p className="shared-chat-dialog-label">聊天记录</p>
                <h2 id={titleId}>{record.title}</h2>
              </div>
              <button ref={closeButton} type="button" onClick={() => setOpen(false)} aria-label="关闭聊天记录">
                关闭
              </button>
            </header>
            <ol className="shared-chat-items">
              {record.items.map((item, index) => (
                <li key={`${item.sender}:${item.sentAt}:${index}`} className="shared-chat-item">
                  <Avatar name={item.sender} url={null} />
                  <div className="shared-chat-item-content">
                    <header>
                      <strong>{item.sender}</strong>
                      <time>{item.sentAt}</time>
                    </header>
                    {item.lines.map((line, lineIndex) => (
                      <SharedChatLine key={`${lineIndex}:${line}`} line={line} />
                    ))}
                  </div>
                </li>
              ))}
            </ol>
          </section>
        </div>
      ) : null}
    </>
  );
}

function previewLine(item: SharedChatItem): string {
  const first = item.lines.find((line) => !line.startsWith("↳ ") && !line.startsWith("链接："));
  return first?.replace(/^【链接】/, "") ?? item.lines[0] ?? "消息";
}

function SharedChatLine({ line }: { line: string }) {
  if (line.startsWith("↳ ")) return <blockquote>{line.slice(2)}</blockquote>;
  if (line.startsWith("链接：")) {
    const url = safeHttpUrl(line.slice("链接：".length));
    return url === null
      ? <p>{line}</p>
      : <a href={url} target="_blank" rel="noreferrer">打开原链接</a>;
  }
  return <p>{line}</p>;
}

function safeHttpUrl(value: string): string | null {
  try {
    const url = new URL(value);
    return url.protocol === "https:" || url.protocol === "http:" ? url.toString() : null;
  } catch {
    return null;
  }
}

function parseContactCard(text: string | null): {
  displayName: string;
  wechatId: string;
  avatarUrl: string | null;
} | null {
  if (text === null || !text.startsWith("[联系人名片] ")) return null;
  const parts = text.slice("[联系人名片] ".length).split(" · ");
  const displayName = parts.find((part) => !part.startsWith("微信号：") && !part.startsWith("头像："));
  const wechatId = parts.find((part) => part.startsWith("微信号："))?.slice("微信号：".length);
  const avatarUrl = parts.find((part) => part.startsWith("头像："))?.slice("头像：".length) ?? null;
  if (displayName === undefined || wechatId === undefined) return null;
  return { displayName, wechatId, avatarUrl };
}

function MediaSummary({ deviceId, message }: { deviceId: string; message: DashboardMessage }) {
  const attachment = message.attachments[0];
  const [mediaUrl, setMediaUrl] = useState<string | null>(null);
  const [imagePreviewOpen, setImagePreviewOpen] = useState(false);
  useEffect(() => {
    if (message.kind === "text" || attachment?.object_state !== "completed" || attachment.object_id === null) {
      return;
    }
    let active = true;
    void getCommunicationObjectReadUrl(
      window.fetch,
      cloudApiOrigin(),
      deviceId,
      attachment.object_id,
    ).then((url) => {
      if (active) setMediaUrl(url);
    }).catch(() => {
      if (active) setMediaUrl(null);
    });
    return () => {
      active = false;
    };
  }, [attachment?.object_id, attachment?.object_state, deviceId, message.kind]);
  useEffect(() => {
    if (!imagePreviewOpen) return;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setImagePreviewOpen(false);
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [imagePreviewOpen]);
  if (attachment === undefined) return <p className="message-attachment">{message.kind} · file unavailable</p>;
  if (message.kind === "image" && mediaUrl !== null) {
    const likelyThumbnail = !attachment.attachment_id.endsWith(":full")
      && attachment.size_bytes < 50 * 1024;
    return (
      <>
        <button
          className="message-image-button"
          type="button"
          title="Double-click to view the uploaded image"
          onDoubleClick={() => setImagePreviewOpen(true)}
        >
          <img className="message-image" src={mediaUrl} alt="Synchronized WeChat image" loading="lazy" />
        </button>
        {likelyThumbnail ? <p className="message-media-quality">Thumbnail cached by WeChat</p> : null}
        {imagePreviewOpen ? (
          <div className="image-preview-overlay" role="dialog" aria-modal="true" aria-label="Image preview" onClick={() => setImagePreviewOpen(false)}>
            <div className="image-preview-content" onClick={(event) => event.stopPropagation()}>
              <div className="image-preview-actions">
                <a href={mediaUrl} target="_blank" rel="noreferrer">Open current image in new tab</a>
                <button type="button" onClick={() => setImagePreviewOpen(false)} aria-label="Close image preview">Close</button>
              </div>
              <img src={mediaUrl} alt="Synchronized WeChat image preview" />
            </div>
          </div>
        ) : null}
      </>
    );
  }
  if (message.kind === "audio" && mediaUrl !== null) {
    return <audio className="message-audio" src={mediaUrl} controls preload="none" />;
  }
  if (message.kind === "video" && mediaUrl !== null) {
    return <video className="message-video" src={mediaUrl} controls preload="metadata" />;
  }
  if (message.kind === "file" && mediaUrl !== null) {
    return <a className="message-file" href={mediaUrl} download>{attachment.file_name ?? "Download file"}</a>;
  }
  return (
    <p className="message-attachment">
      {attachment.file_name ?? message.kind} · {formatBytes(attachment.size_bytes)} · {attachment.object_state ?? "pending upload"}
    </p>
  );
}

function formatBytes(bytes: number): string {
  if (bytes < 1024 ** 2) return `${Math.max(0.1, bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 ** 2)).toFixed(1)} MiB`;
}

function formatTime(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function messageFor(cause: unknown): string {
  return cause instanceof DashboardApiError ? cause.message : "Unable to load messages.";
}
