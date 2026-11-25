"""Apply the exact PRE cleanup plan through an injected staging adapter."""

from pre_target import confirm_container_stopped, plan_cleanup, verify_cleanup


class SweepError(RuntimeError):
    """Cleanup could not be established. Messages are safe for workflow logs."""


def sweep_target(target, adapter, *, now):
    """Sweep one authenticated reservation and return 'spared' or 'cleared'.

    The adapter resolves the reservation's authoritative run status and reads a
    complete, fresh inventory for each call. Its apply method returns True only
    when the named exact operation is confirmed. Before app deletion the
    adapter's stop_container method stops the pinned app and returns fresh
    direct health while the app is still readable. Final inventory preserves
    proved counts and reads resource absence afresh. Startup settlement must
    come from the independent admission authority.
    """
    try:
        status = adapter.run_status(target)
        actions = plan_cleanup(target, adapter.inventory(target), now=now, run_status=status)
    except Exception:
        raise SweepError("PRE cleanup authorization could not be verified") from None
    if actions is None:
        return "spared"

    for action in actions:
        if action.operation == "delete_container":
            try:
                stopped = adapter.stop_container(target, action)
                confirm_container_stopped(target, action, stopped)
            except Exception:
                raise SweepError("PRE container stop could not be verified") from None
        try:
            confirmed = adapter.apply(target, action)
        except Exception:
            raise SweepError("PRE cleanup operation was not confirmed") from None
        if confirmed is not True:
            raise SweepError("PRE cleanup operation was not confirmed")

    try:
        verify_cleanup(target, adapter.inventory(target))
    except Exception:
        raise SweepError("PRE cleanup readback could not be verified") from None
    return "cleared"
