-- Drop the torrent-based acquisition subsystem's tables. All three were exclusively
-- acquisition-owned (0013_acquisition_jobs.sql, 0018_upgrade_scan.sql) and nothing else reads them.
DROP TABLE IF EXISTS acquisition_jobs;
DROP TABLE IF EXISTS upgrade_attempts;
DROP TABLE IF EXISTS upgrade_scan;
