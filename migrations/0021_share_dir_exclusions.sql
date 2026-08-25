-- Folders that are in the library but withheld from ONE person it is shared with.
--
-- Distinct from `library_excluded_dirs` (0006), which is not access control at all: that one keeps
-- folders out of the library for everybody, permanently, by dropping index rows at scan time. This
-- one leaves the files indexed and playable — by the owner and by everyone else — and refuses them
-- to one grantee at stream time.
--
-- ## Why the paths live HERE and not on the Hub
--
-- They are absolute paths on the owner's machine. The Hub is deliberately ignorant of the data plane
-- (it never sees audio bytes), and there is no reason for it to learn a user's directory layout
-- either. The capability token already carries `sub` — the user being authorized — so the library can
-- answer "is this folder withheld from THIS person" entirely on its own, next to the only copy of
-- the paths that has to exist.
--
-- The alternative was a Hub table plus an exclusion list stamped into every token. That would have
-- put filesystem paths in the control plane AND in a signed token, to answer a question the library
-- was better placed to answer.
--
-- `grantee_user_id` is the Hub's user UUID, matched against the token's `sub`. No foreign key: the
-- library has no users table and must not need one.
CREATE TABLE library_share_excluded_dirs (
    library_id      TEXT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    grantee_user_id TEXT NOT NULL,
    path            TEXT NOT NULL,
    PRIMARY KEY (library_id, grantee_user_id, path)
);

-- The stream path reads this per request, keyed by both columns.
CREATE INDEX library_share_excluded_dirs_lookup
    ON library_share_excluded_dirs (library_id, grantee_user_id);
