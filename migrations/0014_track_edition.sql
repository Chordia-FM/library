-- Edition-aware albums: a track from a deluxe/special/expanded edition folds into the BASE album (so
-- "X" and "X (Deluxe)" are one album), with the edition qualifier kept here on the track. NULL = the
-- standard edition. Lets the album view show every edition's tracks together and metrics roll up to
-- the one album. Existing rows are filled on the next re-scan.
ALTER TABLE tracks ADD COLUMN edition TEXT;
