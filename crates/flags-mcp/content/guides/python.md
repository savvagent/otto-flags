# otto-flags Python SDK (app: {{APP_NAME}})

`otto-flags` is the server-side Python SDK: it evaluates flags for a backend service, with a sync
client (`FlagClient`) and an async one (`AsyncFlagClient`). It authenticates with the app's
server key. That key is shown once, by `create_app` or `rotate_app_keys`; keep it in the app's
secret store and read it from the `OTTO_FLAGS_API_KEY` environment variable. Never write it into
code, a config file under version control, or a browser bundle. The distribution is `otto-flags`;
the import name is `otto_flags`. Python 3.9 or later.

## Install

Install from source, pinned to a full commit SHA of `savvagent/otto-flags`. Find the current tip
of `main` with:

```bash
git ls-remote https://github.com/savvagent/otto-flags main
```

and use that SHA (or another one you have checked) in place of `<commit>`:

```bash
pip install "otto-flags @ git+https://github.com/savvagent/otto-flags@<commit>#subdirectory=packages/python-server"
```

or as a dependency in `pyproject.toml`:

```toml
dependencies = [
    "otto-flags @ git+https://github.com/savvagent/otto-flags@<commit>#subdirectory=packages/python-server",
]
```

The same line works in `requirements.txt`. Pin a SHA, not a branch, so a build is reproducible.

## Initialize

Create one client per process and reuse it; it holds the cache, the telemetry queue and the
real-time connection.

Sync:

```python
import os

from otto_flags import FlagClient, FlagClientConfig

config = FlagClientConfig(
    api_key=os.environ["OTTO_FLAGS_API_KEY"],
    application_id="{{APP_ID}}",
    environment="production",
    defaults={"new-checkout": False},
)

client = FlagClient(config)
# ... on shutdown:
client.close()
```

`FlagClient` is also a context manager (`with FlagClient(config) as client:`).

Async (the client must be entered before use, then closed):

```python
import os

from otto_flags import AsyncFlagClient, FlagClientConfig

config = FlagClientConfig(
    api_key=os.environ["OTTO_FLAGS_API_KEY"],
    application_id="{{APP_ID}}",
)

async with AsyncFlagClient(config) as client:
    ...
```

Other `FlagClientConfig` fields, all optional: `base_url` (default
`https://otto-flags.savvagent.com`), `environment` (default `"production"`), `cache_ttl` (seconds,
default 60), `enable_realtime` (default `True`), `enable_telemetry` (default `True`), `timeout`
(seconds, default 5.0), `defaults` (flag key to the value served when evaluation fails), and
`on_error` (a callable that receives SDK errors).

## Evaluate a flag

Pass a `FlagContext` describing who the evaluation is for. Percentage rollouts bucket on
`user_id` (or `anonymous_id`, then `session_id`), so pass a stable identifier.

```python
from otto_flags import FlagContext

ctx = FlagContext(user_id="user-123", attributes={"plan": "pro"})

if client.is_enabled("new-checkout", ctx):
    render_new_checkout()
else:
    render_old_checkout()

result = client.evaluate("new-checkout", ctx)
# result.value (bool), result.variation, result.configuration, result.reason
```

With the async client the same calls are awaited: `await client.is_enabled("new-checkout", ctx)`.

Also available: `get_config(flag_key, context, default=...)` for configuration attached to a flag,
`get_variation(flag_key, context)` for multi-variant flags, and `track_error(flag_key, error,
context)` to report an exception raised in flagged code, which is what `flag_health` and
auto-rollback compare.

Results are cached per flag and per evaluation context (the context as sent) for `cache_ttl`
seconds, so two users never share a cached result. `invalidate_cache()` clears it by hand.

## Frameworks

### FastAPI

Open the async client in the lifespan and hand it out as a dependency:

```python
import os
from contextlib import asynccontextmanager

from fastapi import Depends, FastAPI, Request
from otto_flags import AsyncFlagClient, FlagClientConfig, FlagContext

client: AsyncFlagClient | None = None

@asynccontextmanager
async def lifespan(app: FastAPI):
    global client
    config = FlagClientConfig(
        api_key=os.environ["OTTO_FLAGS_API_KEY"],
        application_id="{{APP_ID}}",
    )
    async with AsyncFlagClient(config) as c:
        client = c
        yield

app = FastAPI(lifespan=lifespan)

def flag_context(request: Request) -> FlagContext:
    return FlagContext(user_id=request.headers.get("X-User-ID"))

@app.get("/checkout")
async def checkout(ctx: FlagContext = Depends(flag_context)):
    return {"newCheckout": await client.is_enabled("new-checkout", ctx)}
```

### Flask

One module-level sync client, closed at exit, and a context per request:

```python
import atexit
import os

from flask import Flask, g, request
from otto_flags import FlagClient, FlagClientConfig, FlagContext

app = Flask(__name__)
client = FlagClient(FlagClientConfig(
    api_key=os.environ["OTTO_FLAGS_API_KEY"],
    application_id="{{APP_ID}}",
))
atexit.register(client.close)

@app.before_request
def attach_flag_context():
    g.flag_context = FlagContext(user_id=request.headers.get("X-User-ID"))

@app.route("/checkout")
def checkout():
    return {"newCheckout": client.is_enabled("new-checkout", g.flag_context)}
```

### Django

Keep the key in settings (read from the environment there) and attach the client and a context in
middleware:

```python
# settings.py
OTTO_FLAGS_API_KEY = os.environ["OTTO_FLAGS_API_KEY"]
MIDDLEWARE = [..., "myapp.middleware.OttoFlagsMiddleware"]

# myapp/middleware.py
from django.conf import settings
from otto_flags import FlagClient, FlagClientConfig, FlagContext

_client = None

def get_client():
    global _client
    if _client is None:
        _client = FlagClient(FlagClientConfig(
            api_key=settings.OTTO_FLAGS_API_KEY,
            application_id="{{APP_ID}}",
        ))
    return _client

class OttoFlagsMiddleware:
    def __init__(self, get_response):
        self.get_response = get_response

    def __call__(self, request):
        request.otto_flags = get_client()
        request.flag_context = FlagContext(
            user_id=str(request.user.id) if request.user.is_authenticated else None,
        )
        return self.get_response(request)

# a view
def checkout(request):
    enabled = request.otto_flags.is_enabled("new-checkout", request.flag_context)
    ...
```

## Notes

- A flag lookup never raises into the app: on a network or server error the SDK returns the value
  from `defaults` (or `False`) with `reason == "error"`, and passes the error to
  `on_error` if set. Only constructing a client with a missing or malformed key raises
  (`InvalidAPIKeyError`), so a misconfigured deploy fails at start-up, not per request.
- Telemetry (evaluation counts and tracked errors) is batched and sent in the background; it feeds
  `flag_health` and auto-rollback. Set `enable_telemetry=False` to turn it off.
- With `enable_realtime` on, the client holds a Server-Sent Events connection and invalidates its
  cache when a flag changes, so a change made with `set_flag_environment` or `rollback_flag`
  reaches the app within moments rather than after `cache_ttl`.
- Create flags before code references them: an unknown flag key evaluates to its default.
- The wire contract the SDK speaks is `docs/SDK-DEVELOPER-GUIDE.md` in the repository.
