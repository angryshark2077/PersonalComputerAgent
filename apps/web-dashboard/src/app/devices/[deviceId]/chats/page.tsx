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
  messagesReadBaselineStorageKey,
  messagesReadStorageKey,
  type CommunicationSource,
  type DashboardConversation,
  type DashboardConversationPage,
} from "../../../../lib/api";
import { getBrowserSession, redirectToSignIn } from "../../../../lib/auth";

export default function DeviceChatsPage() {
  return (
    <CommunicationConversationsPage
      source="communication.wechat"
      rootPath="chats"
      title="WeChat"
      backLabel="Back to WeChat devices"
    />
  );
}

export function CommunicationConversationsPage({
  source,
  rootPath,
  title,
  backLabel,
}: {
  source: CommunicationSource;
  rootPath: "chats" | "messages";
  title: "WeChat" | "Messages";
  backLabel: string;
}) {
  const params = useParams<{ deviceId: string }>();
  const [conversations, setConversations] = useState<DashboardConversation[] | null>(null);
  const [pagination, setPagination] = useState<DashboardConversationPage["pagination"] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [readTimes, setReadTimes] = useState<Record<string, string | null>>({});
  const [page, setPage] = useState(1);

  useEffect(() => {
    setPage(1);
  }, [params.deviceId, source]);

  useEffect(() => {
    const origin = cloudApiOrigin();
    let active = true;
    let loading = false;
    const refresh = async () => {
      if (loading) return;
      loading = true;
      try {
        const latest = await getCommunicationConversations(window.fetch, origin, params.deviceId, source, 100, page);
        if (!active) return;
        const storageKey = source === "communication.wechat" ? chatReadStorageKey : messagesReadStorageKey;
        const baselineKey = source === "communication.wechat"
          ? chatReadBaselineStorageKey(params.deviceId)
          : messagesReadBaselineStorageKey(params.deviceId);
        const baselineInitialized = window.localStorage.getItem(baselineKey) !== null
          || latest.conversations.some((conversation) => window.localStorage.getItem(
            storageKey(params.deviceId, conversation.conversation_id),
          ) !== null);
        const nextReadTimes = Object.fromEntries(latest.conversations.map((conversation) => {
          const key = storageKey(params.deviceId, conversation.conversation_id);
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
        setConversations(latest.conversations);
        setPagination(latest.pagination);
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
  }, [page, params.deviceId, source]);

  return (
    <DashboardShell>
      <Link className="back-link" href={`/${rootPath}`}>{backLabel}</Link>
      <section className="page-heading">
        <p className="workspace-name">Device {params.deviceId}</p>
        <h1>{title}</h1>
        <p>{source === "communication.wechat" ? "WeChat conversations collected on this Mac." : "iMessage and SMS conversations collected on this Mac."}</p>
      </section>
      {error !== null ? <p role="alert">{error}</p> : null}
      {conversations === null ? <p className="status-note">Loading conversations…</p> : (
        <section className="dashboard-panel" aria-labelledby="chats-heading">
          <div className="panel-header">
            <h2 id="chats-heading">Conversations</h2>
            <p className="panel-count">{pagination?.total_count ?? conversations.length} total</p>
          </div>
          {conversations.length === 0 ? <p className="empty-state">No synchronized {title} conversations yet.</p> : (
            <ul className="conversation-list">
              {conversations.map((conversation) => (
                <li key={conversation.conversation_id}>
                  <Link
                    className="conversation-link"
                    href={`/devices/${encodeURIComponent(params.deviceId)}/${rootPath}/${encodeURIComponent(conversation.conversation_id)}?page=${pagination?.page ?? page}`}
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
          {pagination !== null && pagination.total_pages > 1 ? (
            <nav aria-label="Conversation pages">
              <button type="button" disabled={pagination.page === 1} onClick={() => setPage(pagination.page - 1)}>
                Previous
              </button>
              {pageNumbers(pagination.total_pages, pagination.page).map((number, index) => number === null ? (
                <span key={`ellipsis-${index}`} aria-hidden="true">…</span>
              ) : (
                <button
                  key={number}
                  type="button"
                  disabled={number === pagination.page}
                  aria-current={number === pagination.page ? "page" : undefined}
                  onClick={() => setPage(number)}
                >
                  {number}
                </button>
              ))}
              <button type="button" disabled={pagination.page === pagination.total_pages} onClick={() => setPage(pagination.page + 1)}>
                Next
              </button>
            </nav>
          ) : null}
        </section>
      )}
    </DashboardShell>
  );
}

function pageNumbers(totalPages: number, currentPage: number): Array<number | null> {
  if (totalPages <= 7) return Array.from({ length: totalPages }, (_, index) => index + 1);
  const pages: Array<number | null> = [1];
  const firstMiddle = Math.max(2, currentPage - 1);
  const lastMiddle = Math.min(totalPages - 1, currentPage + 1);
  if (firstMiddle > 2) pages.push(null);
  for (let page = firstMiddle; page <= lastMiddle; page += 1) pages.push(page);
  if (lastMiddle < totalPages - 1) pages.push(null);
  pages.push(totalPages);
  return pages;
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
  return cause instanceof DashboardApiError ? cause.message : "Unable to load conversations.";
}
