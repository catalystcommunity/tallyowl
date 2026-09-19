"""Tests for the release verbs.

Every test here is arithmetic on files, so every test here is a unit test. None
of them waits, and none of them needs a cluster: the standing directive after
L172 is that anything a calculation can answer gets a test rather than a wait.

The version-site tests run against copies of the real files rather than against
invented ones. A pattern that matches an invented line and misses the real file
is the failure this module exists to prevent.
"""

from __future__ import annotations

import json
import shutil
import tempfile
import unittest
from pathlib import Path

from tallyowl_tools import release
from tallyowl_tools.commands import REPOSITORY_ROOT, ToolFailed


def _copy_sites(root: Path) -> None:
    """Copy every version site into a temporary tree, keeping the paths."""
    for site in release.SITES:
        source = REPOSITORY_ROOT / site.path
        destination = root / site.path
        destination.parent.mkdir(parents=True, exist_ok=True)
        if not destination.exists():
            shutil.copy(source, destination)


class VersionShape(unittest.TestCase):
    def test_a_release_and_a_candidate_are_versions(self) -> None:
        for version in ("0.1.0", "0.1.0-rc.1", "1.0.0", "10.20.30-rc.12"):
            self.assertIsNotNone(release.VERSION.match(version), version)

    def test_anything_a_registry_reads_differently_is_refused(self) -> None:
        for version in ("0.1", "v0.1.0", "0.1.0-rc1", "0.1.0-alpha", "0.1.0 ", ""):
            self.assertIsNone(release.VERSION.match(version), version)

    def test_set_version_refuses_a_shape_it_cannot_cut(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ToolFailed) as refusal:
                release.set_version("0.1", Path(directory), regenerate=False)
        self.assertIn("three numbers", str(refusal.exception))


class Sites(unittest.TestCase):
    def test_every_site_exists_and_holds_a_version(self) -> None:
        """A site nobody can find is a site nobody checks."""
        for site in release.SITES:
            with self.subTest(site=site.path, what=site.what):
                found = release.read_site(site)
                self.assertTrue(found)
                for version in found:
                    self.assertIsNotNone(release.VERSION.match(version), version)

    def test_the_repository_agrees_with_itself(self) -> None:
        release.check()

    def test_a_missing_site_is_a_refusal_rather_than_a_pass(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(ToolFailed):
                release.read_site(release.SITES[0], Path(directory))

    def test_a_site_that_changed_shape_is_a_refusal(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text("[workspace.package]\nedition = \"2021\"\n")
            with self.assertRaises(ToolFailed) as refusal:
                release.read_site(release.SITES[0], root)
        self.assertIn("SITES", str(refusal.exception))


class Stamping(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        _copy_sites(self.root)
        self.addCleanup(self.directory.cleanup)

    def test_the_source_sites_take_the_new_version(self) -> None:
        release.set_version("9.9.9-rc.7", self.root, regenerate=False)
        for site in release.SITES:
            if site.generated:
                continue
            with self.subTest(site=site.path, what=site.what):
                self.assertEqual(set(release.read_site(site, self.root)), {"9.9.9-rc.7"})

    def test_a_generated_site_is_left_for_the_generator(self) -> None:
        """`csilgen` writes those. A stamp here would drift from the generator."""
        before = release.current_version(self.root)
        release.set_version("9.9.9-rc.7", self.root, regenerate=False)
        for site in release.SITES:
            if site.generated:
                with self.subTest(site=site.path, what=site.what):
                    self.assertEqual(set(release.read_site(site, self.root)), {before})

    def test_a_stamped_package_manifest_is_still_json(self) -> None:
        release.set_version("9.9.9-rc.7", self.root, regenerate=False)
        manifest = self.root / "packages" / "browser" / "package.json"
        self.assertEqual(release.json_version(manifest), "9.9.9-rc.7")
        # The whole file still parses, not only the line the stamp wrote.
        self.assertIn("name", json.loads(manifest.read_text()))

    def test_the_path_dependencies_all_move_together(self) -> None:
        """Fifteen crates, one version. A stale one publishes the wrong crate."""
        release.set_version("9.9.9-rc.7", self.root, regenerate=False)
        site = next(s for s in release.SITES if "path dependencies" in s.what)
        found = release.read_site(site, self.root)
        self.assertEqual(len(found), 15)
        self.assertEqual(set(found), {"9.9.9-rc.7"})

    def test_a_disagreement_names_the_files_that_disagree(self) -> None:
        before = release.current_version(self.root)
        release.set_version("9.9.9-rc.7", self.root, regenerate=False)
        with self.assertRaises(ToolFailed) as refusal:
            release.check(self.root)
        message = str(refusal.exception)
        self.assertIn(before, message)
        self.assertIn("9.9.9-rc.7", message)
        self.assertIn("generated/typescript/tallyowl-ingest-api/package.json", message)

    def test_the_generated_sites_settle_the_disagreement(self) -> None:
        """What `./tools.sh gen` does, done by hand, makes the tree agree."""
        release.set_version("9.9.9-rc.7", self.root, regenerate=False)
        for site in release.SITES:
            if not site.generated:
                continue
            path = self.root / site.path
            written, _ = release.stamp(path.read_text(), site.pattern, "9.9.9-rc.7")
            path.write_text(written)
        release.check(self.root)

    def test_stamping_counts_what_it_wrote(self) -> None:
        text = 'version: 1.2.3\nappVersion: "1.2.3"\n'
        written, count = release.stamp(text, r"(?m)^version: (\S+)$", "2.0.0")
        self.assertEqual(count, 1)
        self.assertEqual(written, 'version: 2.0.0\nappVersion: "1.2.3"\n')


class LockFiles(unittest.TestCase):
    """A lock file holds the version as a key, not as a line."""

    def test_a_lock_file_is_read_by_key(self) -> None:
        site = next(s for s in release.SITES if s.path.endswith("package-lock.json"))
        self.assertIsNotNone(site.json_keys)
        found = release.read_site(site)
        self.assertEqual(len(found), 2, "npm keeps the version at the root and in the entry")
        self.assertEqual(set(found), {release.current_version()})

    def test_a_dependency_version_is_not_mistaken_for_the_package_version(self) -> None:
        """The lock file holds a version for every dependency, at the same
        indentation as the package's own. A pattern that read lines would take
        all of them, and the check would compare TallyOwl against TypeScript."""
        import json

        path = REPOSITORY_ROOT / "packages" / "browser" / "package-lock.json"
        document = json.loads(path.read_text())
        dependencies = [
            entry.get("version")
            for name, entry in document["packages"].items()
            if name and isinstance(entry, dict) and "version" in entry
        ]
        self.assertTrue(dependencies, "the fixture needs a dependency to be meaningful")
        site = next(s for s in release.SITES if s.path.endswith("browser/package-lock.json"))
        for version in release.read_site(site):
            self.assertNotIn(version, dependencies)

    def test_writing_the_version_keeps_the_file_byte_for_byte(self) -> None:
        """npm writes two-space JSON with a trailing newline, and so does this,
        so a version cut leaves no formatting churn behind."""
        import shutil
        import tempfile

        with tempfile.TemporaryDirectory() as directory:
            copy = Path(directory) / "package-lock.json"
            source = REPOSITORY_ROOT / "packages" / "browser" / "package-lock.json"
            shutil.copy(source, copy)
            before = copy.read_text()
            written = release.write_json_version(
                copy, (("version",), ("packages", "", "version")), release.current_version()
            )
            self.assertEqual(written, 2)
            self.assertEqual(copy.read_text(), before)


class Tarballs(unittest.TestCase):
    """L177: a packed tarball that holds only a manifest reaches a registry."""

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.addCleanup(self.directory.cleanup)

    def _pack(self, name: str, members: list[str]) -> Path:
        import tarfile

        tarball = self.root / name
        with tarfile.open(tarball, "w:gz") as bundle:
            for member in members:
                source = self.root / "member"
                source.write_text("{}")
                bundle.add(source, arcname=member)
        return tarball

    def test_a_manifest_and_nothing_else_is_refused(self) -> None:
        self._pack("browser-0.1.0.tgz", ["package/package.json"])
        with self.assertRaises(ToolFailed) as refusal:
            release.check_tarballs(self.root)
        message = str(refusal.exception)
        self.assertIn(release.TARBALL_ENTRY, message)
        self.assertIn("Build the package before it is packed", message)

    def test_a_tarball_with_the_entry_point_passes(self) -> None:
        self._pack(
            "browser-0.1.0.tgz",
            ["package/package.json", release.TARBALL_ENTRY],
        )
        release.check_tarballs(self.root)

    def test_no_tarball_at_all_is_refused(self) -> None:
        with self.assertRaises(ToolFailed):
            release.check_tarballs(self.root)


class Plan(unittest.TestCase):
    """What the release job would do, computed without doing any of it."""

    def setUp(self) -> None:
        self.plan = release.release_plan()

    def test_the_plan_names_a_version_and_a_tag(self) -> None:
        if not self.plan["released"]:
            self.skipTest("no commit since the last tag asks for a release")
        self.assertIsNotNone(release.VERSION.match(self.plan["version"]))
        self.assertEqual(self.plan["tag"], f"v{self.plan['version']}")

    def test_every_go_module_gets_its_own_tag(self) -> None:
        """A Go module in a subdirectory resolves through a prefixed tag."""
        if not self.plan["released"]:
            self.skipTest("no commit since the last tag asks for a release")
        self.assertEqual(len(self.plan["go_tags"]), len(release.GO_MODULES))
        for module, tag in zip(release.GO_MODULES, self.plan["go_tags"]):
            self.assertEqual(tag, f"{module}/v{self.plan['version']}")

    def test_a_go_module_tag_names_a_directory_that_exists(self) -> None:
        for module in release.GO_MODULES:
            self.assertTrue(
                (REPOSITORY_ROOT / module / "go.mod").is_file(),
                f"{module} has no go.mod, so a tag for it would name nothing",
            )

    def test_stamping_outside_the_release_job_changes_nothing(self) -> None:
        """`release stamp` computes on a workstation and stops there.

        It must also not fetch, rebase, or otherwise touch git: a verb somebody
        runs to see what the next version is has no business moving a branch.
        """
        import os

        before = release.current_version()
        was = os.environ.pop(release.RELEASE_ENVIRONMENT, None)
        try:
            release.stamp_release()
        finally:
            if was is not None:
                os.environ[release.RELEASE_ENVIRONMENT] = was
        self.assertEqual(release.current_version(), before)
        self.assertEqual(release.marker()["version"], self.plan["version"])

    def test_tagging_outside_the_release_job_refuses(self) -> None:
        """Nothing tags a version on a workstation, including the tool for it."""
        import os

        was = os.environ.pop(release.RELEASE_ENVIRONMENT, None)
        try:
            with self.assertRaises(ToolFailed) as refusal:
                release.tag_release()
        finally:
            if was is not None:
                os.environ[release.RELEASE_ENVIRONMENT] = was
        self.assertIn("Only the release job tags", str(refusal.exception))


class Binaries(unittest.TestCase):
    """The release page carries the binaries while crates.io is off (L182)."""

    def test_the_archive_names_the_version_and_the_platform(self) -> None:
        self.assertEqual(
            release.binary_archive_name("0.2.0"), "tallyowl-0.2.0-linux-amd64.tar.gz"
        )
        self.assertEqual(
            release.binary_archive_name("0.2.0", "linux-arm64"),
            "tallyowl-0.2.0-linux-arm64.tar.gz",
        )

    def test_a_checksum_line_is_what_sha256sum_writes(self) -> None:
        import hashlib
        import tempfile

        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "tallyowl-0.2.0-linux-amd64.tar.gz"
            path.write_bytes(b"an archive")
            line = release.checksum_line(path)
        digest, name = line.split("  ")
        self.assertEqual(digest, hashlib.sha256(b"an archive").hexdigest())
        self.assertEqual(name, "tallyowl-0.2.0-linux-amd64.tar.gz")

    def test_both_services_are_named(self) -> None:
        """One archive holds the head and the collector, as the image does."""
        self.assertEqual(set(release.BINARIES), {"tallyowl-head", "tallyowl-collector"})


class Tags(unittest.TestCase):
    def setUp(self) -> None:
        self.version = release.current_version()

    def test_the_tag_that_matches_passes(self) -> None:
        release.check_tag(f"v{self.version}")

    def test_a_tag_without_the_v_is_refused(self) -> None:
        with self.assertRaises(ToolFailed) as refusal:
            release.check_tag(self.version)
        self.assertIn("starts with `v`", str(refusal.exception))

    def test_a_tag_for_another_version_is_refused(self) -> None:
        with self.assertRaises(ToolFailed) as refusal:
            release.check_tag("v99.0.0")
        message = str(refusal.exception)
        self.assertIn("v99.0.0", message)
        self.assertIn(self.version, message)

    def test_the_version_of_a_tag(self) -> None:
        self.assertEqual(release.version_of_tag("v0.1.0-rc.1"), "0.1.0-rc.1")
        self.assertEqual(release.version_of_tag("0.1.0-rc.1"), "0.1.0-rc.1")


class CratesPlan(unittest.TestCase):
    def setUp(self) -> None:
        self.plan = release.crates_plan()
        self.names = [crate.name for crate in self.plan]

    def test_the_driver_is_last_because_everything_else_precedes_it(self) -> None:
        self.assertEqual(self.names[-1], release.DRIVER_CRATE)

    def test_a_dependency_is_published_before_the_crate_that_needs_it(self) -> None:
        for crate in ("tallyowl-obs", "tallyowl-wire", "tallyowl-rpc"):
            self.assertLess(self.names.index(crate), self.names.index(release.DRIVER_CRATE))
        self.assertLess(
            self.names.index("tallyowl-ingest-api"), self.names.index("tallyowl-wire")
        )

    def test_the_plan_names_what_the_clearance_has_to_lift(self) -> None:
        """`publish = false` is the flag the owner's clearance lifts (L167)."""
        refused = [crate.name for crate in self.plan if not crate.publishable]
        self.assertIn("tallyowl-wire", refused)

    def test_the_plan_holds_no_crate_twice(self) -> None:
        self.assertEqual(len(self.names), len(set(self.names)))


class Grants(unittest.TestCase):
    def test_nothing_granted_names_every_grant(self) -> None:
        missing = release.missing_grants({})
        self.assertEqual(missing, [name for name, _ in release.GRANTS])

    def test_an_empty_value_counts_as_missing(self) -> None:
        """A resolved-to-nothing secret reference must not read as granted."""
        environment = {name: "   " for name, _ in release.GRANTS}
        self.assertEqual(release.missing_grants(environment), [name for name, _ in release.GRANTS])

    def test_everything_granted_is_nothing_missing(self) -> None:
        environment = {name: "value" for name, _ in release.GRANTS}
        self.assertEqual(release.missing_grants(environment), [])

    def test_the_job_and_the_tooling_want_the_same_grants(self) -> None:
        """The publish job holds the secrets, so its list is the one that runs.

        The two lists are in different files on purpose: everything that
        touches a secret stays in the trusted CI source. This test is what
        keeps them from drifting apart.
        """
        plugin = (
            REPOSITORY_ROOT / ".reactorcide" / "plugins" / "plugin_tallyowl_jobs.py"
        ).read_text()
        job = (REPOSITORY_ROOT / ".reactorcide" / "jobs" / "release.yaml").read_text()
        for name, _ in release.GRANTS:
            self.assertIn(name, plugin, f"{name} is not in the release job's code")
            self.assertIn(name, job, f"{name} is not in the release job definition")

    def test_the_release_job_never_runs_on_a_workstation(self) -> None:
        job = (REPOSITORY_ROOT / ".reactorcide" / "jobs" / "release.yaml").read_text()
        self.assertIn("disable_run_local: true", job)

    def test_the_refusal_says_which_secret_to_create(self) -> None:
        message = release.grant_refusal(release.missing_grants({}))
        self.assertIn("catalystcommunity/registry:user", message)
        self.assertIn("catalystcommunity/ci:githubpat", message)
        self.assertIn("catalystcommunity/ci:npmpublish", message)
        self.assertIn("half-publishing", message)


class Images(unittest.TestCase):
    def test_the_reference_is_the_tag_the_chart_asks_for(self) -> None:
        self.assertEqual(
            release.image_reference("containers.catalystsquad.com/public/catalystcommunity", "0.2.0"),
            "containers.catalystsquad.com/public/catalystcommunity/tallyowl:0.2.0",
        )

    def test_a_trailing_separator_does_not_double(self) -> None:
        self.assertEqual(
            release.image_reference("containers.catalystsquad.com/public/catalystcommunity/", "0.2.0"),
            "containers.catalystsquad.com/public/catalystcommunity/tallyowl:0.2.0",
        )

    def test_the_chart_and_the_release_job_name_one_registry(self) -> None:
        """Two files decide where the image goes, and they must agree."""
        job = (REPOSITORY_ROOT / ".reactorcide" / "jobs" / "release.yaml").read_text()
        registry = release.default_image_registry()
        host, path = registry.split("/", 1)
        self.assertIn(f'REGISTRY: "{host}"', job)
        self.assertIn(f'IMAGE_PATH: "{path}/{release.IMAGE_NAME}"', job)

    def test_the_registry_comes_from_the_chart(self) -> None:
        """One file decides where the image lives, and the chart is that file."""
        registry = release.default_image_registry()
        self.assertEqual(
            release.image_reference(registry, release.current_version()).split(":")[0],
            f"{registry}/tallyowl",
        )

    def test_the_chart_asks_for_the_tag_this_tree_builds(self) -> None:
        chart = REPOSITORY_ROOT / "charts" / "tallyowl" / "Chart.yaml"
        self.assertIn(f'appVersion: "{release.current_version()}"', chart.read_text())


if __name__ == "__main__":
    unittest.main()


class HelmCheckIsWhole(unittest.TestCase):
    """`helm-check` returning 0 must mean it did the work.

    A function that falls off its end returns `None`, which the dispatcher
    reports as success. That happened here: an edit nested the refusal loop
    inside a new function, and `helm-check` passed while checking nothing. The
    verb is a long sequence of steps, so its shape is worth an assertion.
    """

    def test_check_returns_zero_rather_than_falling_off_its_end(self) -> None:
        import inspect

        from tallyowl_tools import helm

        source = inspect.getsource(helm.check)
        self.assertTrue(
            source.rstrip().endswith("return 0"),
            "helm.check must end by returning 0, or a silent skip reads as a pass",
        )

    def test_every_step_of_the_check_is_reached(self) -> None:
        """Each named step appears in the body rather than in a nested function."""
        import inspect

        from tallyowl_tools import helm

        body = inspect.getsource(helm.check)
        for step in (
            "check_rendered_settings",
            "check_rendered_binds",
            "check_gateway_refusals",
            "for chart, refusals in",
        ):
            self.assertIn(step, body, f"helm.check no longer runs {step}")
