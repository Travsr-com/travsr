"""Domain models — definition fixture for travsr-lsif-py tests."""


class User:
    """A user entity."""

    def __init__(self, name: str, email: str) -> None:
        self.name = name
        self.email = email

    def greet(self) -> str:
        return f"Hello, {self.name}"

    def save(self) -> None:
        """Persist the user (no-op in test fixture)."""
        pass


def create_user(name: str, email: str) -> "User":
    """Factory function — used by service.py to test cross-file call edges."""
    return User(name, email)


def validate_email(email: str) -> bool:
    """Validate an email string — imported and called by runner.py."""
    return "@" in email and "." in email
