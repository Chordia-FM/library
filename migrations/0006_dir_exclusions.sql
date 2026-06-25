-- Per-library directory exclusions. A library is a root folder; the owner can untick sub-folders
-- to keep them out of the library. A file is excluded if it lives under any excluded directory.
CREATE TABLE library_excluded_dirs (
    library_id TEXT NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    path       TEXT NOT NULL,
    PRIMARY KEY (library_id, path)
);
