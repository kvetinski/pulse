#!/usr/bin/env python3
"""Choose an unused RFC1918 /24 for an isolated Docker Compose network."""

from __future__ import annotations

import argparse
import ipaddress
import json
import subprocess
import sys
from collections.abc import Iterable


PRIVATE_BLOCKS = (
    ipaddress.ip_network("10.0.0.0/8"),
    ipaddress.ip_network("172.16.0.0/12"),
    ipaddress.ip_network("192.168.0.0/16"),
)


def _run_json(command: list[str]) -> object | None:
    try:
        completed = subprocess.run(
            command,
            check=True,
            capture_output=True,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    try:
        return json.loads(completed.stdout)
    except json.JSONDecodeError:
        return None


def _ipv4_networks(values: Iterable[object]) -> list[ipaddress.IPv4Network]:
    networks: list[ipaddress.IPv4Network] = []
    for value in values:
        if not isinstance(value, str):
            continue
        try:
            network = ipaddress.ip_network(value, strict=False)
        except (TypeError, ValueError):
            continue
        if isinstance(network, ipaddress.IPv4Network):
            networks.append(network)
    return networks


def _docker_subnets(payload: object) -> list[ipaddress.IPv4Network]:
    if not isinstance(payload, list):
        return []

    values: list[object] = []
    for network in payload:
        if not isinstance(network, dict):
            continue
        ipam = network.get("IPAM")
        if not isinstance(ipam, dict):
            continue
        configs = ipam.get("Config")
        if not isinstance(configs, list):
            continue
        for config in configs:
            if isinstance(config, dict):
                values.append(config.get("Subnet"))
    return _ipv4_networks(values)


def existing_network_subnet(network_name: str) -> ipaddress.IPv4Network | None:
    payload = _run_json(["docker", "network", "inspect", network_name])
    if not isinstance(payload, list) or not payload:
        return None
    subnets = _docker_subnets(payload[:1])
    return subnets[0] if subnets else None


def docker_networks() -> list[ipaddress.IPv4Network]:
    listed = subprocess.run(
        ["docker", "network", "ls", "--quiet"],
        check=True,
        capture_output=True,
        text=True,
    )
    network_ids = listed.stdout.split()
    if not network_ids:
        return []
    payload = _run_json(["docker", "network", "inspect", *network_ids])
    if not isinstance(payload, list):
        raise RuntimeError("Docker returned invalid network inspection data")
    return _docker_subnets(payload)


def host_routes() -> list[ipaddress.IPv4Network]:
    payload = _run_json(["ip", "-json", "-4", "route", "show"])
    if not isinstance(payload, list):
        return []
    routes = _ipv4_networks(
        route.get("dst", "") for route in payload if isinstance(route, dict)
    )
    # Full-tunnel VPN clients commonly install two split-default /1 routes.
    # They do not make every RFC1918 subnet unusable by a local Docker bridge;
    # only specific routes contained within a private block are conflicts.
    return [
        route
        for route in routes
        if any(route.subnet_of(private) for private in PRIVATE_BLOCKS)
    ]


def candidates() -> Iterable[ipaddress.IPv4Network]:
    # Prefer the least commonly occupied edge of 172.16/12, then fall back to
    # the other private ranges. Host routes are checked, so a corporate VPN
    # advertising a broad private prefix safely excludes that entire range.
    for second_octet in (31, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 16):
        for third_octet in range(255, -1, -1):
            yield ipaddress.ip_network(
                f"172.{second_octet}.{third_octet}.0/24", strict=True
            )
    for second_octet in range(255, -1, -1):
        for third_octet in range(255, -1, -1):
            yield ipaddress.ip_network(
                f"10.{second_octet}.{third_octet}.0/24", strict=True
            )
    for third_octet in range(255, -1, -1):
        yield ipaddress.ip_network(f"192.168.{third_octet}.0/24", strict=True)


def select_subnet(
    occupied: Iterable[ipaddress.IPv4Network],
) -> ipaddress.IPv4Network:
    used = list(occupied)
    for candidate in candidates():
        if all(not candidate.overlaps(network) for network in used):
            return candidate
    raise RuntimeError("no unused RFC1918 /24 remains for the Compose network")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--network-name",
        required=True,
        help="reuse this Docker network's subnet when it already exists",
    )
    args = parser.parse_args()

    try:
        existing = existing_network_subnet(args.network_name)
        subnet = existing or select_subnet([*docker_networks(), *host_routes()])
    except (OSError, subprocess.CalledProcessError, RuntimeError) as error:
        print(f"cannot select Docker Compose subnet: {error}", file=sys.stderr)
        return 1

    print(subnet)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
