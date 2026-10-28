-- Preserve existing libraries' missing-policy semantics. The migration hook
-- replaces this marker and seeds Off only for an entirely empty new library,
-- in this same schema transaction before user_version is committed.
INSERT INTO app_meta (key, value, updated_at)
VALUES ('summary_destination_bootstrap', 'legacy', 0);
