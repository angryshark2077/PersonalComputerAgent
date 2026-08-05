"use client";

import { CommunicationConversationsPage } from "../chats/page";

export default function DeviceMessagesPage() {
  return (
    <CommunicationConversationsPage
      source="communication.messages"
      rootPath="messages"
      title="Messages"
      backLabel="Back to Messages devices"
    />
  );
}
