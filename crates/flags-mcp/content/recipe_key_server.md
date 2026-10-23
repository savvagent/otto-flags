3. **Hand off the server key, then stop.** This SDK reads the app's secret server key.
   `create_app` and `rotate_app_keys` return it once; it is not stored in clear and cannot be shown
   again, and it passes through this conversation. You cannot set secrets. After `create_app`,
   stop and ask the person to store the key as `OTTO_FLAGS_API_KEY` before you wire any code, in
   each place the app runs: the local `.env` (git-ignored), the CI secrets, and the deploy
   platform's secrets. Never write it into code, a committed file or a later message. If they did
   not store it, or it was exposed, call `rotate_app_keys` (owner or admin; the old keys stop
   working) and hand the new key off the same way.
