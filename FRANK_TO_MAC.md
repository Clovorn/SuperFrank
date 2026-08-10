# Gap7-P0 Status: BLOCKED

## Completed
✅ Added `run_v14_migrations()` function to `/opt/frankos/runtime/frankos-gateway/src/db.rs`
✅ Code review: migration creates `frank_refresh_tokens` table with proper indexes

## Blocked By
❌ Build fails due to permission issue with `/opt/frankos/runtime/frankos-gateway/src/engineer.rs`
   - File is owned by `root:root` with `-rw------- (600)` permissions
   - Build user `frankos` cannot read the file
   - Cannot use sudo in this environment

## Required Actions (Mac Frank)
1. Fix permissions: `sudo chown frankos:frankos /opt/frankos/runtime/frankos-gateway/src/engineer.rs`
   OR: `sudo chmod 644 /opt/frankos/runtime/frankos-gateway/src/engineer.rs`

2. After permissions fixed, run build:
   ```bash
   cd /opt/frankos/runtime/frankos-gateway
   cargo build --release
   ```

3. Add call to `run_v14_migrations(pool).await?;` in `main.rs` after `run_v13_migrations` call
   (main.rs is a protected file per task spec)

4. Deploy with:
   ```bash
   /opt/frankos/bin/deploy.sh gap7-p0-refresh-token-migration
   ```

5. Verify:
   ```bash
   grep -c "run_v14_migrations" src/db.rs  # should be >= 1
   curl -s http://127.0.0.1:8080/health    # should return {"status":"ok",...}
   ```

## Migration Code Added
The `run_v14_migrations()` function creates:
- `frank_refresh_tokens` table with columns: id, user_id, token_hash, expires_at, revoked_at, created_at
- Foreign key constraint to `frankos_users(id)` with CASCADE delete
- Unique constraint on `token_hash`
- Index on `user_id`
- Index on `token_hash`

This supports Gap 7 (refresh token rotation) for secure authentication.
