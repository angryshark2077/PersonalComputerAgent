ALTER TABLE pairing_state ADD COLUMN cloud_api_origin TEXT NOT NULL
    DEFAULT 'https://pca-cloud-api-production.up.railway.app'
    CHECK (
        cloud_api_origin GLOB 'https://*'
        AND length(cloud_api_origin) <= 512
        AND instr(cloud_api_origin, '?') = 0
        AND instr(cloud_api_origin, '#') = 0
    );
