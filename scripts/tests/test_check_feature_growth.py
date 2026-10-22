"""Focused tests for the feature-surface resolver in `check-feature-growth.py`."""

from __future__ import annotations

import gc
import importlib.util
import unittest
import warnings
from unittest.mock import patch
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "check-feature-growth.py"
SPEC = importlib.util.spec_from_file_location("check_feature_growth", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


class ResolverTests(unittest.TestCase):
    """Covers dep/feature, local-name, and optional-dependency edges."""

    def setUp(self) -> None:
        self.workspace = CHECK.Workspace(CHECK.REPO_ROOT)

    def test_manifest_emits_no_resource_warning(self) -> None:
        with warnings.catch_warnings(record=True) as recorded:
            warnings.simplefilter("always", ResourceWarning)
            self.workspace.manifest(CHECK.UMBRELLA)
            gc.collect()
        self.assertEqual(
            [str(warning.message) for warning in recorded if warning.category is ResourceWarning],
            [],
        )

    def test_manifest_closes_its_file_on_success_and_parse_error(self) -> None:
        original_open = open
        try:
            CHECK.tomllib.loads("broken = [")
        except CHECK.tomllib.TOMLDecodeError as error:
            parse_error = error
        for fail in (False, True):
            with self.subTest(parse_error=fail):
                workspace = CHECK.Workspace(CHECK.REPO_ROOT)
                opened = []

                def record_open(*args, **kwargs):
                    handle = original_open(*args, **kwargs)
                    opened.append(handle)
                    return handle

                try:
                    with patch("builtins.open", side_effect=record_open) as opener:
                        if fail:
                            with patch.object(CHECK.tomllib, "load", side_effect=parse_error):
                                with self.assertRaises(CHECK.tomllib.TOMLDecodeError) as raised:
                                    workspace.manifest(CHECK.UMBRELLA)
                            self.assertIs(raised.exception, parse_error)
                            self.assertNotIn(CHECK.UMBRELLA, workspace._manifests)
                        else:
                            manifest = workspace.manifest(CHECK.UMBRELLA)
                            self.assertIs(workspace.manifest(CHECK.UMBRELLA), manifest)
                            self.assertIn("features", manifest)
                        opener.assert_called_once_with(
                            CHECK.REPO_ROOT / CHECK.UMBRELLA / "Cargo.toml", "rb"
                        )
                    self.assertEqual(len(opened), 1)
                    self.assertTrue(opened[0].closed, "the manifest file must close deterministically")
                finally:
                    for handle in opened:
                        handle.close()

    def test_local_feature_edge_is_followed(self) -> None:
        # graph-sqlite names the local `graph` feature, which pulls adk-graph.
        deps = self.workspace.deps_of(CHECK.UMBRELLA, "graph-sqlite")
        self.assertIn("adk-graph", deps)

    def test_dep_feature_edge_reaches_the_forwarded_feature(self) -> None:
        # graph-sqlite forwards to adk-graph/sqlite, which enables sqlx.
        deps = self.workspace.deps_of(CHECK.UMBRELLA, "graph-sqlite")
        self.assertIn("sqlx", deps)

    def test_canonical_feature_is_a_superset_of_its_aliases(self) -> None:
        openai = self.workspace.deps_of(CHECK.UMBRELLA, "openai")
        fireworks = self.workspace.deps_of(CHECK.UMBRELLA, "fireworks")
        self.assertEqual(openai, fireworks)

    def test_unknown_crate_resolves_to_empty(self) -> None:
        self.assertEqual(self.workspace.deps_of("does-not-exist", "nope"), frozenset())

    def test_recursion_terminates(self) -> None:
        # A feature cycle must not hang the resolver.
        self.workspace._manifests["cycle"] = {"features": {"a": ["b"], "b": ["a"]}}
        self.assertEqual(self.workspace.deps_of("cycle", "a"), frozenset())


class SurfaceTests(unittest.TestCase):
    """Covers the gate against the real workspace surface."""

    def setUp(self) -> None:
        self.result = CHECK.analyse(CHECK.Workspace(CHECK.REPO_ROOT))

    def test_current_surface_is_recorded(self) -> None:
        self.assertEqual(CHECK.check(self.result), 0)

    def test_every_alias_has_a_decision(self) -> None:
        for name in self.result["aliases"]:
            self.assertIn(
                name,
                CHECK.ALIASES,
                f"{name} shares a dependency effect and has no recorded decision",
            )

    def test_decision_record_names_only_real_features(self) -> None:
        for name in CHECK.ALIASES:
            self.assertIn(name, self.result["features"])

    def test_unrecorded_duplicate_is_rejected(self) -> None:
        self.result["aliases"]["cli-telemetry-otlp"] = ["cli"]
        self.assertEqual(CHECK.check(self.result), 1)

    def test_unrecorded_feature_without_dependencies_is_rejected(self) -> None:
        self.result["features"]["do-nothing"] = []
        self.result["deps"]["do-nothing"] = frozenset()
        self.assertEqual(CHECK.check(self.result), 1)

    def test_axes_partition_the_surface(self) -> None:
        seen: set[str] = set()
        for axis in self.result["axes"]:
            self.assertFalse(seen & set(self.result["axes"][axis]))
            seen |= set(self.result["axes"][axis])
        self.assertEqual(seen, set(self.result["features"]))

    def test_tier_features_are_classified_as_tiers(self) -> None:
        for name in CHECK.TIERS:
            self.assertEqual(CHECK.axis_of(name), "tier")
