-- Renames the built-in, non-removable "Default" collection to
-- "Master Collection". Only runs the rename if it's actually safe to:
--   - a collection literally named "Default" exists, AND
--   - nothing named "Master Collection" already exists (so this can't
--     collide with a collection someone already created by that name and
--     silently merge two different collections' cards together).
--
-- If those conditions aren't both true, this migration is a no-op and
-- leaves existing collections exactly as they are -- new installs get the
-- default collection created under the new name from the start, since this
-- runs immediately after the 01-collections migration that creates it.
UPDATE collection
SET name = 'Master Collection'
WHERE name = 'Default'
  AND NOT EXISTS (SELECT 1 FROM collection WHERE name = 'Master Collection');

UPDATE cards
SET collection = 'Master Collection'
WHERE collection = 'Default'
  AND NOT EXISTS (
    SELECT 1 FROM collection WHERE name = 'Default'
  );
