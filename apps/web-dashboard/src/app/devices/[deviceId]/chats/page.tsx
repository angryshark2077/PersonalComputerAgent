"use client";

import Link from "next/link";
import { useParams } from "next/navigation";
import { useEffect, useState } from "react";

import { DashboardShell } from "../../../../components/dashboard-shell";
import {
  DashboardApiError,
  chatReadStorageKey,
  cloudApiOrigin,
  getCommunicationConversations,
  isConversationUnread,
  type DashboardConversation,
} from "../../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../../lib/auth";

export default function DeviceChatsPage() {
  const params = useParams<{ deviceId: string }>();
  const [conversations, setConversations] = useState<DashboardConversation[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [readTimes, setReadTimes] = useState<Record<string, string | null>>({});

  useEffect(() => {
    if (conversations === null) return;
    const refreshReadTimes = () => setReadTimes(Object.fromEntries(conversations.map((conversation) => [
      conversation.conversation_id,
      window.localStorage.getItem(chatReadStorageKey(params.deviceId, conversation.conversation_id)),
    ])));
    window.addEventListener("focus", refreshReadTimes);
    window.addEventListener("pageshow", refreshReadTimes);
    return () => {
      window.removeEventListener("focus", refreshReadTimes);
      window.removeEventListener("pageshow", refreshReadTimes);
    };
  }, [conversations, params.deviceId]);

  useEffect(() => {
    const origin = cloudApiOrigin();
    void (async () => {
      if ((await getBrowserSession(window.fetch, origin)) === null) {
        redirectToSignIn();
        return;
      }
      try {
        const latest = await getCommunicationConversations(window.fetch, origin, params.deviceId);
        setConversations(latest);
        setReadTimes(Object.fromEntries(latest.map((conversation) => [
          conversation.conversation_id,
          window.localStorage.getItem(chatReadStorageKey(params.deviceId, conversation.conversation_id)),
        ])));
      } catch (cause) {
        setError(messageFor(cause));
      }
    })();
  }, [params.deviceId]);

  return (
    <DashboardShell>
      <Link className="back-link" href="/chats">Back to chat devices</Link>
      <section className="page-heading">
        <p className="workspace-name">Device {params.deviceId}</p>
        <h1>Chats</h1>
        <p>Conversations from the WeChat account collected on this Mac.</p>
      </section>
      {error !== null ? <p role="alert">{error}</p> : null}
      {conversations === null ? <p className="status-note">Loading chats…</p> : (
        <section className="dashboard-panel" aria-labelledby="chats-heading">
          <div className="panel-header">
            <h2 id="chats-heading">Conversations</h2>
            <p className="panel-count">{conversations.length} total</p>
          </div>
          {conversations.length === 0 ? <p className="empty-state">No synchronized chats yet.</p> : (
            <ul className="conversation-list">
              {conversations.map((conversation) => (
                <li key={conversation.conversation_id}>
                  <Link
                    className="conversation-link"
                    href={`/devices/${encodeURIComponent(params.deviceId)}/chats/${encodeURIComponent(conversation.conversation_id)}`}
                  >
                    <div className="conversation-main">
                      <p className="conversation-title">{conversationTitle(conversation)}</p>
                      {isConversationUnread(
                        conversation.last_message_at,
                        readTimes[conversation.conversation_id] ?? null,
                      ) ? <span className="unread-dot" aria-label="Unread messages" /> : null}
                      <p className="conversation-id">{conversation.conversation_id}</p>
                    </div>
                    <div className="conversation-meta">
                      <div>{conversation.message_count} messages</div>
                      <time dateTime={conversation.last_message_at}>{formatTime(conversation.last_message_at)}</time>
                    </div>
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </section>
      )}
    </DashboardShell>
  );
}

function conversationTitle(conversation: DashboardConversation): string {
  return conversation.scope === "group"
    ? `${conversation.display_name} · ${conversation.member_count ?? "?"} members`
    : conversation.display_name;
}

function formatTime(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function messageFor(cause: unknown): string {
  return cause instanceof DashboardApiError ? cause.message : "Unable to load chats.";
}
