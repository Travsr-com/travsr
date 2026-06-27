"""Service layer — cross-file call fixture for travsr-lsif-py tests.

Tests resolved:
  from .models import create_user   →  direct import  (fn:create_user in models.py)
  from .models import validate_email →  direct import  (fn:validate_email in models.py)
  create_user(...)                  →  RefCall edge to models.py:fn:create_user
  validate_email(...)               →  RefCall edge to models.py:fn:validate_email
"""

from .models import create_user, validate_email


def register_user(name: str, email: str):
    """Register a new user after validating the email address."""
    if not validate_email(email):
        raise ValueError(f"Invalid email: {email}")
    return create_user(name, email)


def batch_register(users):
    """Register a list of (name, email) pairs."""
    return [register_user(name, email) for name, email in users]
