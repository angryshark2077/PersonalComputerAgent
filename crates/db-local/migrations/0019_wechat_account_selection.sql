ALTER TABLE applied_collector_control
    ADD COLUMN communication_wechat_account_id TEXT;

CREATE TRIGGER applied_collector_control_wechat_account_insert_guard
BEFORE INSERT ON applied_collector_control
WHEN NEW.communication_wechat_account_id IS NOT NULL
 AND (length(NEW.communication_wechat_account_id) <> 64
      OR NEW.communication_wechat_account_id GLOB '*[^0-9a-f]*')
BEGIN
    SELECT RAISE(ABORT, 'invalid WeChat account selector');
END;

CREATE TRIGGER applied_collector_control_wechat_account_update_guard
BEFORE UPDATE OF communication_wechat_account_id ON applied_collector_control
WHEN NEW.communication_wechat_account_id IS NOT NULL
 AND (length(NEW.communication_wechat_account_id) <> 64
      OR NEW.communication_wechat_account_id GLOB '*[^0-9a-f]*')
BEGIN
    SELECT RAISE(ABORT, 'invalid WeChat account selector');
END;
