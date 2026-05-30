# Priority Playlist Quality Test Design

## Goal

Add lightweight quality testing for priority IPTV playlists. When app identifies playlist as priority, sample 5 random-ish channels from matching priority live categories (`US`, `Usa`, `locals`) and verify streams look media-valid.

Keep complexity low:

- No `ffmpeg`/`ffprobe`
- No heavy dependencies
- Prefer existing `reqwest` + Tokio
- Quality test priority playlists only

## Architecture

Run quality test inside `process_playlist`, after priority decision. Existing app already fetches categories and live streams count; change flow to preserve enough metadata for sampling.

Add structs:

- `LiveCategory { category_id, category_name }`
- `LiveStream { name, stream_id, category_id, stream_type, container_extension }`
- `QualityTest { enabled, sample_size, candidates, tested, passed, failed, pass_rate, channels }`
- `QualityProbeResult { name, stream_id, category_name, url, ok, status, content_type, bytes_read, reason }`

Refactor helpers:

- replace/augment `fetch_category_names` with `fetch_live_categories`
- add `fetch_live_streams`
- derive `live_channel_categories: Vec<String>` from category structs
- derive `live_channels_supported` from `live_streams.len()`

Priority category IDs come from live categories where existing matching logic passes:

- category contains exact `US`
- category contains `Usa`
- lowercase category contains `locals`

Candidates are live streams whose `category_id` matches one of those IDs.

## Sampling

Default sample size: 5.

Avoid `rand` dependency. Use deterministic pseudo-random selection:

- hash playlist identity + stream id/name using std hashing
- sort candidate streams by hash
- take first 5

Benefits:

- no new crate
- stable/reproducible results
- varied enough across playlists

If fewer than 5 candidates exist, test all candidates.

## Probe Behavior

Build Xtream stream URL:

```text
{server}/live/{username}/{password}/{stream_id}.{container_extension}
```

Use `container_extension`, fallback `ts`.

For each sample, perform HTTP GET with:

- `Range: bytes=0-65535`
- timeout: 8 seconds
- existing user agent/client behavior

Pass conditions:

- request succeeds
- status is success or `206 Partial Content`
- body non-empty and preferably greater than 188 bytes
- response looks media-valid by content type or bytes

Accepted signals:

- HLS: content type `application/vnd.apple.mpegurl` or `application/x-mpegurl`, or body starts with `#EXTM3U`
- MPEG-TS: content type `video/mp2t`, or TS sync byte pattern (`0x47`) at expected packet intervals
- MP4: content type `video/mp4`, or bytes contain `ftyp` near header
- Octet stream: `application/octet-stream` accepted as probable if body has enough bytes

Failures record reason:

- request error
- HTTP status failure
- empty body
- unknown content

## Output JSON

Add nullable field to `PlaylistResult`:

```json
"quality_test": {
  "enabled": true,
  "sample_size": 5,
  "candidates": 123,
  "tested": 5,
  "passed": 4,
  "failed": 1,
  "pass_rate": 0.8,
  "channels": [
    {
      "name": "US ESPN",
      "stream_id": 12345,
      "category_name": "US Sports",
      "url": "http://server/live/user/pass/12345.ts",
      "ok": true,
      "status": 206,
      "content_type": "video/mp2t",
      "bytes_read": 65536,
      "reason": "mpeg_ts"
    }
  ]
}
```

For non-priority playlists, `quality_test` should be `null` or omitted. Prefer `Option<QualityTest>` with `#[serde(skip_serializing_if = "Option::is_none")]` to avoid clutter.

Note: output already stores credentials. Stream URLs include same credentials, so no new security class.

## Runtime / Complexity

Expected complexity: modest.

Runtime cost:

- priority playlist only
- max 5 extra HTTP requests per priority playlist
- serial probe first for simplicity
- worst case: 5 x 8s = 40s extra per bad priority playlist

Avoid concurrency initially. If slow, later add bounded Tokio concurrency.

## Test Plan

Unit tests only; no live network tests required.

Add tests for:

1. priority category ID extraction
2. deterministic sampler returns max 5 and stable order
3. stream URL builder uses extension fallback
4. media sniffing:
   - HLS `#EXTM3U` passes
   - TS sync byte passes
   - MP4 `ftyp` passes
   - octet-stream with enough bytes passes as probable
   - empty body fails
   - HTML body fails

Run:

```bash
cargo test
```
