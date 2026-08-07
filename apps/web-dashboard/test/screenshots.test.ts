import assert from "node:assert/strict";
import test from "node:test";

import { getScreenshots, SCREENSHOT_PAGE_SIZE } from "../src/lib/api.ts";

test("screenshot pages request one numbered page at a time", async () => {
  let requested = "";
  await getScreenshots(async (input) => {
    requested = String(input);
    return Response.json({ screenshots: [], next_cursor: null });
  }, "https://cloud.example", "device", SCREENSHOT_PAGE_SIZE, 3);

  assert.equal(SCREENSHOT_PAGE_SIZE, 20);
  assert.equal(
    requested,
    "https://cloud.example/v1/devices/device/screenshots?limit=20&page=3",
  );
});
