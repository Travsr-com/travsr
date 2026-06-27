"""Runner — tests module-level import (import x → x.func()) call edges.

Tests resolved:
  import models                     →  module entry (models → models.py)
  models.create_user(...)           →  attribute RefCall edge to models.py:fn:create_user
  models.validate_email(...)        →  attribute RefCall edge to models.py:fn:validate_email
"""

import models  # noqa: F401 — relative import not available in standalone run


def run_demo():
    """Entry point that exercises module-attribute-style calls."""
    valid = models.validate_email("alice@example.com")
    if valid:
        user = models.create_user("Alice", "alice@example.com")
        return user
    return None
