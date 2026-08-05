import assert from "node:assert/strict";
import test from "node:test";

import { nextPhotoPage, PHOTO_PAGE_SIZE } from "../src/lib/api.ts";

test("photo pages expose only twenty new records at a time", () => {
  const records = Array.from({ length: 45 }, (_, index) => index);

  assert.equal(PHOTO_PAGE_SIZE, 20);
  assert.deepEqual(nextPhotoPage(records, 0), records.slice(0, 20));
  assert.deepEqual(nextPhotoPage(records, 20), records.slice(20, 40));
  assert.deepEqual(nextPhotoPage(records, 40), records.slice(40, 45));
});
