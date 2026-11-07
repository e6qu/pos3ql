import importlib.util
from pathlib import Path
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "ownership_manifest", ROOT / "tools/miri-ownership-manifest.py"
)
MANIFEST = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MANIFEST)


class OwnershipManifestTests(unittest.TestCase):
    def test_every_real_proof_belongs_to_exactly_one_nonempty_group(self):
        names = MANIFEST.read_manifest(ROOT / "tools/miri-ownership-tests.txt")
        selected = [MANIFEST.select(names, group) for group in MANIFEST.GROUPS]
        flattened = [name for group in selected for name in group]
        self.assertCountEqual(flattened, names)
        self.assertEqual(len(flattened), len(set(flattened)))

    def test_rejects_empty_duplicate_partial_and_unassigned_names(self):
        for text in ("", "mem::test\nmem::test\n", "partial\n", "other::test\n"):
            with self.subTest(text=text), tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "manifest"
                path.write_text(text)
                with self.assertRaises(ValueError):
                    MANIFEST.read_manifest(path)

    def test_empty_group_cannot_succeed(self):
        with self.assertRaises(ValueError):
            MANIFEST.select(["sql::test"], "storage")

    def test_accepts_each_selected_proof_once_including_expected_panics(self):
        MANIFEST.verify(
            "test mem::one ... ok\n"
            "test sql::two - should panic ... ok\n"
            "test result: ok. 2 passed; 0 failed; 0 ignored; 4 measured; 3 filtered out;\n",
            ["mem::one", "sql::two"],
        )

    def test_rejects_missing_extra_duplicate_and_ignored_proofs(self):
        for log in (
            "test mem::one ... ok\n"
            "test result: ok. 1 passed; 0 failed; 0 ignored;\n",
            "test mem::one ... ok\ntest sql::two ... ok\ntest storage::extra ... ok\n"
            "test result: ok. 3 passed; 0 failed; 0 ignored;\n",
            "test mem::one ... ok\ntest mem::one ... ok\ntest sql::two ... ok\n"
            "test result: ok. 3 passed; 0 failed; 0 ignored;\n",
            "test mem::one ... ok\ntest sql::two ... ignored\n"
            "test result: ok. 1 passed; 0 failed; 1 ignored;\n",
        ):
            with self.subTest(log=log), self.assertRaises(ValueError):
                MANIFEST.verify(log, ["mem::one", "sql::two"])

    def test_test_lines_do_not_qualify_without_one_matching_success_summary(self):
        for ending in (
            "",
            "test result: FAILED. 1 passed; 1 failed; 0 ignored;\n",
            "test result: ok. 2 passed; 0 failed; 0 ignored;\n",
            "test result: ok. 1 passed; 0 failed; 0 ignored;\n" * 2,
        ):
            with self.subTest(ending=ending), self.assertRaises(ValueError):
                MANIFEST.verify("test mem::one ... ok\n" + ending, ["mem::one"])


if __name__ == "__main__":
    unittest.main()
