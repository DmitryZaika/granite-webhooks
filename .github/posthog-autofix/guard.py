"""PreToolUse hook for the auto-fix agent (wired in settings.json).

Reads the hook event (JSON) from stdin and exits 2 with a one-line reason on
stderr to block the tool call, 0 to allow it. Fail-closed: any error here,
including failing to import the policy, blocks the call.
"""

import json
import os
import sys

BLOCK = 2


def main() -> int:
    try:
        sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
        from autofix.guard_policy import decide

        event = json.load(sys.stdin)
        if not isinstance(event, dict):
            raise ValueError("the hook input is not a JSON object")
        # Set by the agent step; the agent never works in GITHUB_WORKSPACE.
        reason = decide(event, os.environ.get("AUTOFIX_WORKSPACE", ""))
    except Exception as error:  # fail closed on anything
        reason = f"the guard failed ({error.__class__.__name__}), so the call is blocked"
    if reason is None:
        return 0
    print(f"Blocked by the auto-fix guard: {reason}", file=sys.stderr)
    return BLOCK


if __name__ == "__main__":
    sys.exit(main())
