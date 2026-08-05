"use client";

import { CommunicationMessagesPage } from "../../chats/[conversationId]/page";

export default function MessagesConversationPage() {
  return <CommunicationMessagesPage source="communication.messages" rootPath="messages" />;
}
