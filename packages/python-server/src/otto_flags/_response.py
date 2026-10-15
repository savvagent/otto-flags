"""Reading the evaluate response (docs/SDK-DEVELOPER-GUIDE.md, Flag Evaluation)."""

from __future__ import annotations

from typing import Any

from .exceptions import OttoFlagsError


def evaluate_payload(data: Any) -> tuple[dict[str, Any], bool]:
    """Return the response object and its ``enabled`` flag, or raise ``OttoFlagsError``.

    The server answers ``{"key", "enabled", "variation", "configuration", ...}``. A body that is
    not an object, or whose ``enabled`` is missing or not a JSON boolean, is a failed lookup: the
    caller returns the default and caches nothing (#112). Substituting the default here would
    cache it for the TTL as if the server had said so.
    """
    if not isinstance(data, dict):
        raise OttoFlagsError("the evaluate response is not a JSON object")
    enabled = data.get("enabled")
    if not isinstance(enabled, bool):
        raise OttoFlagsError("the evaluate response has no boolean `enabled`")
    return data, enabled
