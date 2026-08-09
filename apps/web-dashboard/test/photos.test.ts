import assert from "node:assert/strict";
import test from "node:test";

import { getPhotos, PHOTO_PAGE_SIZE } from "../src/lib/api.ts";

test("photo pages request one numbered server page at a time", async () => {
  let requested = "";
  const fetcher = async (input: RequestInfo | URL) => {
    requested = String(input);
    return new Response(JSON.stringify({
      photos: [],
      pagination: { page: 3, page_size: 20, total_count: 45, total_pages: 3 },
    }), { status: 200, headers: { "content-type": "application/json" } });
  };

  assert.equal(PHOTO_PAGE_SIZE, 20);
  const result = await getPhotos(fetcher, "", "device-1", undefined, 3);
  assert.match(requested, /\/v1\/devices\/device-1\/photos\?limit=20&page=3$/);
  assert.equal(result.pagination.total_count, 45);
});
