"use client";

import Link from "next/link";
import { useParams } from "next/navigation";
import { useCallback, useEffect, useRef, useState } from "react";

import { DashboardShell } from "../../../../../components/dashboard-shell";
import {
  DashboardApiError,
  chatReadStorageKey,
  cloudApiOrigin,
  decodeDashboardRouteParam,
  getCommunicationConversations,
  getCommunicationMessages,
  type DashboardMessage,
} from "../../../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../../../lib/auth";

export default function ChatMessagesPage() {
  const params = useParams<{ deviceId: string; conversationId: string }>();
  const deviceId = decodeDashboardRouteParam(params.deviceId);
  const conversationId = decodeDashboardRouteParam(params.conversationId);
  const [messages, setMessages] = useState<DashboardMessage[] | null>(null);
  const [displayName, setDisplayName] = useState(conversationId);
  const [conversationScope, setConversationScope] = useState<"direct" | "group">("direct");
  const [hasOlderMessages, setHasOlderMessages] = useState(false);
  const [loadingOlderMessages, setLoadingOlderMessages] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const messagePanel = useRef<HTMLElement | null>(null);
  const didInitialScroll = useRef(false);
  const loadingOlderMessagesRef = useRef(false);

  useEffect(() => {
    const origin = cloudApiOrigin();
    void (async () => {
      if ((await getBrowserSession(window.fetch, origin)) === null) {
        redirectToSignIn();
        return;
      }
      try {
        const [latest, conversations] = await Promise.all([
          getCommunicationMessages(
            window.fetch,
            origin,
            deviceId,
            conversationId,
          ),
          getCommunicationConversations(window.fetch, origin, deviceId),
        ]);
        const conversation = conversations.find(
          (candidate) => candidate.conversation_id === conversationId,
        );
        setDisplayName(conversation?.display_name ?? conversationId);
        setConversationScope(conversation?.scope ?? "direct");
        setHasOlderMessages(latest.length === 100);
        setMessages(latest.toReversed());
        window.localStorage.setItem(
          chatReadStorageKey(deviceId, conversationId),
          conversation?.last_message_at ?? new Date().toISOString(),
        );
      } catch (cause) {
        setError(messageFor(cause));
      }
    })();
  }, [conversationId, deviceId]);

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
  }, [conversationId, deviceId, hasOlderMessages, messages]);

  return (
    <DashboardShell>
      <Link className="back-link" href={`/devices/${encodeURIComponent(deviceId)}/chats`}>Back to chats</Link>
      <section className="page-heading">
        <h1>{displayName}</h1>
        <p className="conversation-id">{conversationId}</p>
      </section>
      {error !== null ? <p role="alert">{error}</p> : null}
      {messages === null ? <p className="status-note">Loading messages…</p> : (
        <section
          ref={messagePanel}
          className="dashboard-panel message-scroll"
          aria-label="Conversation messages"
          onScroll={(event) => {
            if (event.currentTarget.scrollTop <= 40) void loadOlderMessages();
          }}
        >
          {loadingOlderMessages ? <p className="message-page-status">Loading older messages…</p> : null}
          {!hasOlderMessages && messages.length > 0
            ? <p className="message-page-status">Beginning of synchronized history</p>
            : null}
          {messages.length === 0 ? <p className="empty-state">No synchronized messages.</p> : (
            <ol className="message-list">
              {messages.map((message) => (
                <li className={`message-row is-${message.direction}`} key={message.event_id}>
                  <article className="message-bubble">
                    {conversationScope === "group"
                      ? <p className="message-sender">{message.sender_display_name}</p>
                      : null}
                    {message.kind === "text"
                      ? <p className="message-text">{message.text}</p>
                      : <MediaSummary message={message} />}
                    <p className="message-meta">
                      {message.direction === "incoming" ? "Received" : "Sent"} · {formatTime(message.occurred_at)}
                    </p>
                  </article>
                </li>
              ))}
            </ol>
          )}
        </section>
      )}
    </DashboardShell>
  );
}

function MediaSummary({ message }: { message: DashboardMessage }) {
  const attachment = message.attachments[0];
  if (attachment === undefined) return <p className="message-attachment">{message.kind} · file unavailable</p>;
  return (
    <p className="message-attachment">
      {message.kind} · {(attachment.size_bytes / (1024 ** 2)).toFixed(1)} MiB · {attachment.object_state ?? "pending upload"}
    </p>
  );
}

function formatTime(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function messageFor(cause: unknown): string {
  return cause instanceof DashboardApiError ? cause.message : "Unable to load messages.";
}
