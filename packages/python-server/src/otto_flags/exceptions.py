"""
Otto Flags Python Server SDK Exceptions
"""


class OttoFlagsError(Exception):
    """Base exception for Otto Flags SDK."""

    pass


class InvalidAPIKeyError(OttoFlagsError):
    """Raised when the API key is invalid or missing."""

    pass


class APIError(OttoFlagsError):
    """Raised when the API returns an error response."""

    def __init__(self, status_code: int, message: str) -> None:
        self.status_code = status_code
        self.message = message
        super().__init__(f"API error {status_code}: {message}")


class AuthenticationError(APIError):
    """Raised when authentication fails (401/403)."""

    def __init__(self, message: str = "Authentication failed") -> None:
        super().__init__(401, message)


class RateLimitError(APIError):
    """Raised when rate limit is exceeded (429)."""

    def __init__(self, message: str = "Rate limit exceeded") -> None:
        super().__init__(429, message)


class TimeoutError(OttoFlagsError):
    """Raised when a request times out."""

    pass


class ConnectionError(OttoFlagsError):
    """Raised when unable to connect to the API."""

    pass


class ConfigurationError(OttoFlagsError):
    """Raised when configuration is invalid."""

    pass
