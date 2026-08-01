from __future__ import annotations

import importlib.util
import ipaddress
from pathlib import Path
import unittest


MODULE_PATH = Path(__file__).resolve().parents[1] / "select_subnet.py"
SPEC = importlib.util.spec_from_file_location("select_subnet", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
SELECTOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SELECTOR)


class SelectSubnetTests(unittest.TestCase):
    def test_prefers_deterministic_high_private_subnet(self) -> None:
        selected = SELECTOR.select_subnet([])
        self.assertEqual(selected, ipaddress.ip_network("172.31.255.0/24"))

    def test_skips_any_overlapping_broad_route(self) -> None:
        selected = SELECTOR.select_subnet(
            [ipaddress.ip_network("172.31.0.0/16")]
        )
        self.assertEqual(selected, ipaddress.ip_network("172.29.255.0/24"))

    def test_skips_specific_docker_network(self) -> None:
        selected = SELECTOR.select_subnet(
            [ipaddress.ip_network("172.31.255.0/24")]
        )
        self.assertEqual(selected, ipaddress.ip_network("172.31.254.0/24"))

    def test_split_default_routes_do_not_exhaust_private_candidates(self) -> None:
        broad_routes = [
            ipaddress.ip_network("0.0.0.0/1"),
            ipaddress.ip_network("128.0.0.0/1"),
        ]
        relevant = [
            route
            for route in broad_routes
            if any(route.subnet_of(private) for private in SELECTOR.PRIVATE_BLOCKS)
        ]
        self.assertEqual(relevant, [])
        self.assertEqual(
            SELECTOR.select_subnet(relevant),
            ipaddress.ip_network("172.31.255.0/24"),
        )


if __name__ == "__main__":
    unittest.main()
