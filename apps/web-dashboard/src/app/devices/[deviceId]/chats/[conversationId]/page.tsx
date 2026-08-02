"use client";

import Link from "next/link";
import { useParams } from "next/navigation";
import { useEffect, useState } from "react";

import { DashboardShell } from "../../../../../components/dashboard-shell";
import {
  DashboardApiError,
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
  const [error, setError] = useState<string | null>(null);

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
        setDisplayName(
          conversations.find(
            (conversation) => conversation.conversation_id === conversationId,
          )?.display_name ?? conversationId,
        );
        setMessages(latest.toReversed());
      } catch (cause) {
        setError(messageFor(cause));
      }
    })();
  }, [conversationId, deviceId]);

  return (
    <DashboardShell>
      <Link className="back-link" href={`/devices/${encodeURIComponent(deviceId)}/chats`}>Back to chats</Link>
      <section className="page-heading">
        <p className="workspace-name">Latest 100 synchronized messages</p>
        <h1>{displayName}</h1>
        <p className="conversation-id">{conversationId}</p>
      </section>
      {error !== null ? <p role="alert">{error}</p> : null}
      {messages === null ? <p className="status-note">Loading messages…</p> : (
        <section className="dashboard-panel" aria-label="Conversation messages">
          {messages.length === 0 ? <p className="empty-state">No synchronized messages.</p> : (
            <ol className="message-list">
              {messages.map((message) => (
                <li className={`message-row is-${message.direction}`} key={message.event_id}>
                  <article className="message-bubble">
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
