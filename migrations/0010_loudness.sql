-- EBU R128 / ReplayGain 2.0 loudness, computed by a background pass (library/src/loudness.rs).
-- Stored on `files` (a property of the audio content, keyed by content_hash) so a re-index of the
-- same bytes preserves the analysis. NULL until analyzed.
ALTER TABLE files ADD COLUMN rg_gain_db REAL; -- track gain in dB, reference -18 LUFS
ALTER TABLE files ADD COLUMN rg_peak    REAL; -- linear true-peak amplitude (≈0..1+)
