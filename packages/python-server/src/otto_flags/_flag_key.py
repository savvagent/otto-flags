"""Flag key validation shared by the sync and async clients."""

from __future__ import annotations

import re

_FLAG_KEY = re.compile(r"[A-Za-z0-9._-]{1,128}")


def is_valid_flag_key(key: str) -> bool:
    """Return True if ``key`` can be sent as a flag key.

    Mirrors the server's key rule: 1-128 characters, each in ``[A-Za-z0-9._-]``,
    and not ``.`` or ``..``. The whole string must match (no trimming), so a
    trailing newline is rejected. ``.`` and ``..`` are rejected because HTTP
    clients and servers resolve them as path segments, so they cannot be
    carried in a URL path. A key with ``/``, ``?`` or ``#`` would otherwise
    steer the request, and the SDK key it carries, to another endpoint (#107).
    """
    if not isinstance(key, str):
        return False
    if key in (".", ".."):
        return False
    return _FLAG_KEY.fullmatch(key) is not None


def describe_flag_key(key: str) -> str:
    """Describe a rejected key for an error message, truncated to 64 characters."""
    return f"{key[:64]!r} ({len(key)} characters)"
