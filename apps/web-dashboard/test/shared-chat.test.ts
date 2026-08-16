import assert from "node:assert/strict";
import test from "node:test";

import { parseSharedChatRecord } from "../src/lib/shared-chat.ts";

test("parses every forwarded chat item instead of the WeChat preview only", () => {
  const record = parseSharedChatRecord(
    "[聊天记录] 群聊的聊天记录\n"
      + "[完整记录 · 5 条]\n"
      + "── 詹涛 · 2026-08-15 19:55 ──\n"
      + "今天一万多营业额是有了，不过女装退货率特别高\n"
      + "── Linda-晓鑫 · 2026-08-15 19:58 ──\n"
      + "还没收到货 就退货！\n"
      + "↳ 詹涛: 今天一万多营业额是有了，不过女装退货率特别高\n"
      + "── Linda-晓鑫 · 2026-08-15 19:58 ──\n"
      + "？\n"
      + "── 詹涛 · 2026-08-15 20:02 ──\n"
      + "预估退货率百分之40-50\n"
      + "── 是汤姆呀🌞 · 2026-08-15 22:25 ──\n"
      + "【链接】视频标题\n"
      + "视频作者\n"
      + "链接：https://b23.tv/example",
  );

  assert.equal(record?.title, "群聊的聊天记录");
  assert.equal(record?.declaredItemCount, 5);
  assert.equal(record?.items.length, 5);
  assert.deepEqual(record?.items.at(-1), {
    sender: "是汤姆呀🌞",
    sentAt: "2026-08-15 22:25",
    lines: ["【链接】视频标题", "视频作者", "链接：https://b23.tv/example"],
  });
});

test("does not reinterpret an old preview or ordinary text as a complete record", () => {
  assert.equal(parseSharedChatRecord("ordinary text"), null);
  assert.equal(parseSharedChatRecord("[聊天记录] 群聊的聊天记录 · 只有预览"), null);
});
