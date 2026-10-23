3. **Hand off the server key, then stop.** This SDK reads the app's secret server key.
   `create_app` and `rotate_app_keys` return it once; it is not stored in clear and cannot be shown
   again, and it passes through this conversation. You cannot set secrets. If you just ran
   `create_app`, stop and ask the person to store the key as `OTTO_FLAGS_API_KEY` before you wire
   any code, in each place the app runs: the local `.env` (git-ignored), the CI secrets, and the
   deploy platform's secrets. Never write it into code, a committed file or a later message. If
   the app already existed, ask the person where its key is stored and reuse it. Call
   `rotate_app_keys` with `which: "server"` (owner or admin) only if the person approves it after you tell them that
   the old server key stops working at once, so every service running on it needs the new key, and
   hand the new key off the same way. Never rotate on your own because you do not have the key.
