"""Regression tests for release blocking and changelog preparation."""

from pathlib import Path
import tempfile
import unittest

import changelog


class ChangelogTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "docs").mkdir()
        self.path = self.root / "docs/CHANGELOG.md"
        self.set_version("1.2.3")
        self.path.write_text(
            "# Pyroshine changelog\n\n## [Unreleased]\n\n"
            "### Fixed\n\n- Next release fix.\n\n"
            "## [v1.2.3] - 2026-09-30\n\n### Added\n\n- Current feature.\n\n"
            "## [v1.2.2] - 2026-09-29\n\n### Fixed\n\n- Older fix.\n"
        )

    def set_version(self, version):
        (self.root / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')

    def test_notes_only_include_selected_release(self):
        self.assertEqual(
            changelog.release_notes(self.root, "v1.2.3"),
            "### Added\n\n- Current feature.\n",
        )

    def test_tag_must_match_workspace_version(self):
        with self.assertRaisesRegex(ValueError, "does not match"):
            changelog.release_notes(self.root, "v1.2.4")

    def test_version_bump_without_entry_blocks_release(self):
        self.set_version("1.2.4")
        with self.assertRaisesRegex(ValueError, "latest dated"):
            changelog.release_notes(self.root)

    def test_empty_or_placeholder_notes_block_release(self):
        for notes in ["", "- TODO", "- TBD"]:
            with self.subTest(notes=notes):
                text = "## [Unreleased]\n\n## [v1.2.3] - 2026-09-30\n\n" + notes
                self.path.write_text(text)
                with self.assertRaisesRegex(ValueError, "non-placeholder"):
                    changelog.release_notes(self.root)

    def test_malformed_or_duplicate_entries_block_release(self):
        original = self.path.read_text()
        for text in [
            original.replace("2026-09-30", "2026-02-30"),
            original.replace("2026-09-30", "30-09-2026"),
            original.replace("v1.2.2", "v1.2.3"),
        ]:
            with self.subTest(text=text):
                self.path.write_text(text)
                with self.assertRaises(ValueError):
                    changelog.release_notes(self.root)

    def test_prepare_preserves_history_and_moves_unreleased(self):
        self.set_version("1.2.4")
        old_history = self.path.read_text().split("## [v1.2.3]", 1)[1]
        self.assertEqual(changelog.prepare(self.root, "2026-10-01"), "v1.2.4")
        text = self.path.read_text()
        self.assertEqual(text.split("## [v1.2.3]", 1)[1], old_history)
        self.assertEqual(changelog.sections(text)["Unreleased"][2], "")
        self.assertIn("Next release fix.", changelog.release_notes(self.root, "v1.2.4"))
        self.assertIn("## [v1.2.4] - 2026-10-01", text)

    def test_invalid_preparation_does_not_modify_file(self):
        original = self.path.read_text()
        with self.assertRaisesRegex(ValueError, "already has"):
            changelog.prepare(self.root, "2026-10-01")
        self.assertEqual(self.path.read_text(), original)
        self.set_version("1.2.4")
        self.path.write_text(original.replace("- Next release fix.", ""))
        before = self.path.read_text()
        with self.assertRaisesRegex(ValueError, "Unreleased"):
            changelog.prepare(self.root, "2026-10-01")
        self.assertEqual(self.path.read_text(), before)

    def test_new_tags_require_semver_but_preserve_legacy_history(self):
        self.path.write_text(self.path.read_text() + "\n## [v0.16.1.1] - 2026-09-28\n\n- Legacy notes.\n")
        changelog.release_notes(self.root)
        for tag in ["v1.2.3.4", "v01.2.3", "v1.2.3-01"]:
            with self.subTest(tag=tag), self.assertRaisesRegex(ValueError, "SemVer"):
                changelog.release_notes(self.root, tag)
        self.set_version("1.2.4-rc.1")
        changelog.prepare(self.root, "2026-10-01")
        changelog.release_notes(self.root, "v1.2.4-rc.1")


if __name__ == "__main__":
    unittest.main()
