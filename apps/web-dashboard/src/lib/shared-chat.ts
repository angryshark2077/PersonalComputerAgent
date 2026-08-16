export interface SharedChatItem {
  sender: string;
  sentAt: string;
  lines: string[];
}

export interface SharedChatRecord {
  title: string;
  declaredItemCount: number;
  items: SharedChatItem[];
}

const ITEM_HEADER = /^── (.+) · (.+) ──$/;
const RECORD_COUNT = /^\[完整记录 · (\d+) 条\]$/;

export function parseSharedChatRecord(text: string | null): SharedChatRecord | null {
  if (text === null) return null;
  const lines = text.split(/\r?\n/);
  const titlePrefix = "[聊天记录] ";
  if (!lines[0]?.startsWith(titlePrefix)) return null;
  const count = lines[1]?.match(RECORD_COUNT);
  if (count === null || count === undefined) return null;
  const title = lines[0].slice(titlePrefix.length).trim();
  const declaredItemCount = Number.parseInt(count[1] ?? "", 10);
  if (title.length === 0 || !Number.isSafeInteger(declaredItemCount) || declaredItemCount < 1) return null;

  const items: SharedChatItem[] = [];
  let current: SharedChatItem | null = null;
  for (const line of lines.slice(2)) {
    const header = line.match(ITEM_HEADER);
    if (header !== null) {
      if (current !== null && current.lines.length > 0) items.push(current);
      current = {
        sender: header[1]?.trim() ?? "",
        sentAt: header[2]?.trim() ?? "",
        lines: [],
      };
      continue;
    }
    if (current !== null && line.trim().length > 0) current.lines.push(line);
  }
  if (current !== null && current.lines.length > 0) items.push(current);
  if (items.length === 0 || items.some((item) => item.sender.length === 0 || item.sentAt.length === 0)) return null;
  return { title, declaredItemCount, items };
}
