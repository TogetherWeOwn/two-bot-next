"""Preflight the hosted Discord fixture boundary before PRE provisioning."""


if __name__ == "__main__":
    raise SystemExit(
        "pre_hosted_discord_transport_unavailable: no approved boundary injects "
        "DISCORD_GATEWAY_URL and DISCORD_API_BASE into the archived Worker's Container "
        "with same-namespace fixtures and public Discord egress denied on every start"
    )
