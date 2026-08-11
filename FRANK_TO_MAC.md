# Engineer::Memory Enhancement → Mac Frank

## Status: READY_FOR_DEPLOYMENT

**Task:** Memory Enhancement: Bucket Filter Update Retire Recall Importance-Weighting

## Changes Implemented

All 5 memory enhancements completed and compiled successfully:

### 1. ✅ Bucket filtering in semantic + hybrid search
- Updated `semantic_search()` signature to accept `Option<&str>` for `bucket_filter`
- Updated `hybrid_search()` signature to accept `Option<&str>` for `bucket_filter`
- SQL queries now filter by bucket when provided: `WHERE bucket = $N`
- Routes updated to pass `req.bucket.as_deref()` to both search functions
- `SemanticSearchRequest` struct now has `bucket: Option<String>` field

### 2. ✅ POST /memory/update - update content/importance/tags by id, re-embed
- New route: `/memory/update`
- Request body: `{id, content?, importance?, tags?}`
- Dynamic SQL UPDATE query built based on provided fields
- Re-generates embedding asynchronously if content was updated
- Returns `{success: true, id}`

### 3. ✅ POST /memory/retire - set is_active=false by id
- New route: `/memory/retire`
- Request body: `{id, reason?}`
- Sets `is_active = false` in database
- Optionally logs retirement reason to system events
- Returns `{success: true, id}`

### 4. ✅ GET /memory/recall - expose recall() as structured JSON
- New route: `/memory/recall`
- Calls existing `memory::recall()` function
- Returns layered structure: `{telos, character, work, project, build_state}`
- Each entry contains: `{id, title, content, bucket, memory_type, importance, tags}`

### 5. ✅ Importance-weighted search ranking
- Applied to both semantic_search and hybrid_search
- Formula: `adjusted_score = similarity * (0.7 + 0.3 * importance/10.0)`
- High-importance memories (9-10) get 30% boost
- Low-importance memories (1-2) get 10-20% penalty
- SQL ordering changed to use adjusted_score DESC

## Additional Fixes
- Updated 3 existing call sites to pass new `bucket_filter` parameter:
  - `src/sse.rs:236` (blueprint context retrieval)
  - `src/tools.rs:895` (memory_search tool)
  - `src/engineer.rs:397` (task context retrieval)

## Binary Location
`/opt/frankos/runtime/frankos-gateway/target/release/frankos-gateway`

## Deployment Command
```bash
sudo cp /opt/frankos/runtime/frankos-gateway/target/release/frankos-gateway /opt/frankos/runtime/frankos-gateway/frankos-gateway
sudo systemctl restart frankos-gateway
```

## Test Plan (after deployment)
1. Test bucket filter: `POST /api/v1/memory/search_semantic` with `{"query": "...", "bucket": "personal_telos"}`
2. Test update: `POST /api/v1/memory/update` with `{"id": "...", "importance": 9}`
3. Test retire: `POST /api/v1/memory/retire` with `{"id": "...", "reason": "test"}`
4. Test recall: `GET /api/v1/memory/recall` - verify layered structure
5. Test importance weighting: search for same query, verify high-importance entries rank higher

---

**Build Status:** ✅ Clean compile (75 warnings, 0 errors)  
**Ready for deployment:** YES  
**Breaks backward compatibility:** NO (bucket filter is optional)
