"use client";

import Link from "next/link";
import { useParams } from "next/navigation";
import { useEffect, useState } from "react";

import { DashboardShell } from "../../../../components/dashboard-shell";
import {
  DashboardApiError,
  chatReadBaselineStorageKey,
  chatReadStorageKey,
  cloudApiOrigin,
  getCommunicationConversations,
  initializeChatReadAt,
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
    const origin = cloudApiOrigin();
    let active = true;
    let loading = false;
    const refresh = async () => {
      if (loading) return;
      loading = true;
      try {
        const latest = await getCommunicationConversations(window.fetch, origin, params.deviceId);
        if (!active) return;
        const baselineKey = chatReadBaselineStorageKey(params.deviceId);
        const baselineInitialized = window.localStorage.getItem(baselineKey) !== null
          || latest.some((conversation) => window.localStorage.getItem(
            chatReadStorageKey(params.deviceId, conversation.conversation_id),
          ) !== null);
        const nextReadTimes = Object.fromEntries(latest.map((conversation) => {
          const key = chatReadStorageKey(params.deviceId, conversation.conversation_id);
          const storedReadAt = window.localStorage.getItem(key);
          const readAt = initializeChatReadAt(
            conversation.last_message_at,
            storedReadAt,
            baselineInitialized,
          );
          if (storedReadAt === null && readAt !== null) window.localStorage.setItem(key, readAt);
          return [conversation.conversation_id, readAt];
        }));
        window.localStorage.setItem(baselineKey, "1");
        setConversations(latest);
        setReadTimes(nextReadTimes);
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
      await refresh();
    })();
    const refreshOnVisible = () => void refresh();
    const interval = window.setInterval(refreshOnVisible, 15_000);
    window.addEventListener("focus", refreshOnVisible);
    window.addEventListener("pageshow", refreshOnVisible);
    return () => {
      active = false;
      window.clearInterval(interval);
      window.removeEventListener("focus", refreshOnVisible);
      window.removeEventListener("pageshow", refreshOnVisible);
    };
  }, [params.deviceId]);

  return (
    <DashboardShell>
      <Link className="back-link" href="/chats">Back to chat devices</Link>
      <section className="page-heading">
        <p className="workspace-name">Device {params.deviceId}</p>
        <h1>Chats</h1>
        <p>Conversations from WeChat and Apple Messages collected on this Mac.</p>
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
                    <div className="conversation-identity">
                      <Avatar
                        name={conversation.display_name}
                        url={conversation.avatar_url}
                      />
                      <div className="conversation-main">
                        <p className="conversation-title">{conversationTitle(conversation)}</p>
                        {isConversationUnread(
                          conversation.last_message_at,
                          readTimes[conversation.conversation_id] ?? null,
                        ) ? <span className="unread-dot" aria-label="Unread messages" /> : null}
                        <p className="conversation-id">{conversation.conversation_id}</p>
                      </div>
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

function Avatar({ name, url }: { name: string; url: string | null }) {
  return url === null ? (
    <span className="chat-avatar is-placeholder" aria-hidden="true">{initial(name)}</span>
  ) : (
    <img className="chat-avatar" src={url} alt="" loading="lazy" referrerPolicy="no-referrer" />
  );
}

function initial(name: string): string {
  return Array.from(name.trim())[0]?.toUpperCase() ?? "?";
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
