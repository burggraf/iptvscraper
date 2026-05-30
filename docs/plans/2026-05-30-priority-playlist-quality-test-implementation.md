# Priority Playlist Quality Test Implementation Plan

> **REQUIRED SUB-SKILL:** Use the executing-plans skill to implement this plan task-by-task.

**Goal:** Add lightweight media-sniff quality tests for priority playlists only.

**Architecture:** Preserve live category IDs and live stream rows, sample up to 5 streams from priority categories, probe first bytes with existing reqwest client, and serialize quality results into priority playlist JSON.

**Tech Stack:** Rust 2024, Tokio, reqwest, serde/serde_json, existing unit tests.

---

### Task 1: Add test coverage for pure helpers

**Files:**
- Modify: `src/main.rs`

**Step 1:** Add failing tests for priority category ID extraction, deterministic sampler, URL builder, and media sniffing.

**Step 2:** Run `cargo test` and verify new tests fail because helper functions/types do not exist.

### Task 2: Implement pure helper types/functions

**Files:**
- Modify: `src/main.rs`

**Step 1:** Add `LiveCategory`, `LiveStream`, `QualityTest`, and `QualityProbeResult` structs.

**Step 2:** Add helpers: `priority_category_ids`, `sample_streams`, `stream_url`, `sniff_media`, value conversion helpers.

**Step 3:** Run `cargo test` and verify helper tests pass.

### Task 3: Wire network flow

**Files:**
- Modify: `src/main.rs`

**Step 1:** Add `fetch_live_categories`, `fetch_live_streams`, `quality_test_playlist`, and `probe_stream`.

**Step 2:** Change `process_playlist` to fetch categories/streams once, compute priority, and run quality test only for priority playlists.

**Step 3:** Run `cargo test`.

### Task 4: Final verification

**Files:**
- Modify: `src/main.rs`
- Modify: `docs/plans/2026-05-30-priority-playlist-quality-test-implementation.md`

**Step 1:** Run `cargo fmt`.

**Step 2:** Run `cargo test`.

**Step 3:** Inspect `git diff`.
