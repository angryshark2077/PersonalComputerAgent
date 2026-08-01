CREATE TRIGGER IF NOT EXISTS attachment_spool_requires_deterministic_file_name
BEFORE INSERT ON attachment_spool
FOR EACH ROW
WHEN NEW.spool_relative_path <> NEW.sha256
  OR length(NEW.spool_relative_path) <> 64
  OR NEW.spool_relative_path GLOB '*[^0-9a-f]*'
BEGIN
    SELECT RAISE(ABORT, 'attachment spool path must equal its lowercase SHA-256 filename');
END;
