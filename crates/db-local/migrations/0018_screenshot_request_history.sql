CREATE TABLE handled_screenshot_requests (
    request_id TEXT PRIMARY KEY NOT NULL CHECK(length(request_id) = 36),
    handled_at_ms INTEGER NOT NULL CHECK(handled_at_ms > 0)
) STRICT;

CREATE INDEX handled_screenshot_requests_handled_at_idx
ON handled_screenshot_requests(handled_at_ms DESC, request_id DESC);
