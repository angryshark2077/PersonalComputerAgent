ALTER TABLE collector_configs
    ADD COLUMN wechat_account_id text;

ALTER TABLE collector_configs
    ADD CONSTRAINT collector_configs_wechat_account_id_valid
    CHECK (wechat_account_id IS NULL OR wechat_account_id ~ '^[0-9a-f]{64}$');

ALTER TABLE devices
    ADD COLUMN wechat_accounts jsonb NOT NULL DEFAULT '[]'::jsonb;

ALTER TABLE devices
    ADD CONSTRAINT devices_wechat_accounts_array
    CHECK (jsonb_typeof(wechat_accounts) = 'array');
